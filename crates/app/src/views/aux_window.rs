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

//! The window Quick Look, Properties and Settings share: our own title bar (a drag area, the
//! title, a close button) over a body, in its own OS window.
//!
//! Design: `.claude/docs/design/r10-everyday.md` 7.2 and `r1-redesign.md` 6. [`open`] restores
//! the bounds saved in the settings file under the window's key, moved back onto a display that still exists, and
//! saves them again 500 ms after they stop changing. Closing the main window closes every
//! auxiliary window ([`register_main`], [`window_closed`]).
//!
//! The frame's root carries the [`AUX_WINDOW`] key context, so Escape and Ctrl+W close the
//! window. A body that binds Escape in a context of its own (Quick Look) is deeper in the
//! context stack and wins.
//!
//! Platforms: on Windows the bar is the caption (`WindowControlArea::Drag`) and the close button
//! is a `Close` control area, so Windows itself acts on the click. On macOS the transparent
//! title bar drags natively and the system's traffic lights close the window. Elsewhere the bar
//! asks the compositor to move the window and the close button has a click handler.

use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, Global,
    IntoElement, MouseButton, Pixels, Point, Render, SharedString, Size, Stateful, Subscription,
    Task, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowKind, WindowOptions, div, point, prelude::*, px, size, svg,
};

use crate::assets::icon;
use crate::keymap::{self, AUX_WINDOW};
use crate::state::settings::{SettingValue, Settings};
use crate::theme::{Colors, Theme, fonts, metrics, scale::text};

// TODO(R5-platform): Linux has not been run. With server-side decorations the system draws a
// bar of its own above ours; the design for that case is not written.

/// How long the bounds must stop changing before they are remembered.
pub const BOUNDS_SAVE_DELAY: Duration = Duration::from_millis(500);

/// Each new window of a cascading kind opens this far down and right of the newest one.
pub const CASCADE_OFFSET: Pixels = px(24.);

/// A remembered window is on a display when at least this much of it overlaps one: the width
/// of a grabbable piece of its title bar...
const REACH_WIDTH: Pixels = px(100.);
/// ... and the height of the title bar.
const REACH_HEIGHT: Pixels = px(32.);

/// What a caller asks of [`open`].
pub struct AuxOptions {
    /// The key its bounds are remembered under: "window.quick_look", "window.settings",
    /// "window.properties".
    pub bounds_key: &'static str,
    /// Shown in the title bar and handed to the system as the window's title.
    pub title: SharedString,
    /// The size the first window opens at.
    pub size: Size<Pixels>,
    /// The smallest size the window can be resized to.
    pub min_size: Size<Pixels>,
    /// Properties: each new window opens [`CASCADE_OFFSET`] down and right of the newest open
    /// one with the same key, and only the size is remembered.
    pub cascade: bool,
}

// ---------------------------------------------------------------------------------------------
// Remembered bounds. They live in the settings file under the window's key ("window.main",
// "window.quick_look", "window.settings", "window.properties"), as `x,y,width,height` in whole
// pixels (`r10-everyday.md` 8.3, "In the file").

/// The text a bounds is stored as.
fn bounds_text(bounds: Bounds<Pixels>) -> String {
    let whole = |value: Pixels| f32::from(value).round() as i64;
    format!(
        "{},{},{},{}",
        whole(bounds.origin.x),
        whole(bounds.origin.y),
        whole(bounds.size.width),
        whole(bounds.size.height)
    )
}

/// The bounds `text` stores, or `None` for anything that is not four whole numbers with a size
/// above zero. The file is user-editable, so nothing here trusts it.
fn parse_bounds(text: &str) -> Option<Bounds<Pixels>> {
    /// Far beyond any display; keeps the arithmetic on the numbers safe.
    const LIMIT: i64 = 1_000_000;
    let mut parts = text.split(',').map(|part| part.trim().parse::<i64>().ok());
    let (x, y, width, height) = (
        parts.next()??,
        parts.next()??,
        parts.next()??,
        parts.next()??,
    );
    if parts.next().is_some() {
        return None;
    }
    let sane = |value: i64| value.abs() <= LIMIT;
    if !(sane(x) && sane(y) && sane(width) && sane(height)) || width <= 0 || height <= 0 {
        return None;
    }
    let px_of = |value: i64| px(value as f32);
    Some(Bounds::new(
        point(px_of(x), px_of(y)),
        size(px_of(width), px_of(height)),
    ))
}

/// The bounds last remembered under `key`.
pub fn remembered(cx: &App, key: &'static str) -> Option<Bounds<Pixels>> {
    cx.try_global::<Settings>()
        .and_then(|settings| settings.text_of(key))
        .and_then(parse_bounds)
}

/// Remembers `bounds` under `key`, in the settings file.
pub fn remember(cx: &mut App, key: &'static str, bounds: Bounds<Pixels>) {
    if cx.try_global::<Settings>().is_none() {
        return;
    }
    Settings::set(cx, key, SettingValue::Text(bounds_text(bounds)));
}

// ---------------------------------------------------------------------------------------------
// Open windows.

/// Every open auxiliary window, oldest first, and the main window they belong to.
#[derive(Default)]
struct AuxWindows {
    open: Vec<(AnyWindowHandle, &'static str)>,
    main: Option<AnyWindowHandle>,
}

impl Global for AuxWindows {}

/// Records the main window. When it closes, every auxiliary window closes with it.
pub fn register_main(cx: &mut App, main: AnyWindowHandle) {
    cx.default_global::<AuxWindows>().main = Some(main);
}

/// The main window, while it is open.
pub fn main_window(cx: &App) -> Option<AnyWindowHandle> {
    let main = cx.try_global::<AuxWindows>()?.main?;
    cx.windows().contains(&main).then_some(main)
}

/// Closes every auxiliary window.
pub fn close_all(cx: &mut App) {
    let open = std::mem::take(&mut cx.default_global::<AuxWindows>().open);
    for (handle, key) in open {
        handle
            .update(cx, |_, window, cx| {
                remember(cx, key, window.window_bounds().get_bounds());
                window.remove_window();
            })
            .ok();
    }
}

/// Closes the window being updated, remembering where it was. Every close inside the app goes
/// through this, so the last resize is never lost to the 500 ms delay.
pub fn close_window(window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    let key = cx
        .default_global::<AuxWindows>()
        .open
        .iter()
        .find_map(|(open, key)| (*open == handle).then_some(*key));
    if let Some(key) = key {
        remember(cx, key, window.window_bounds().get_bounds());
    }
    window.remove_window();
}

/// The rule for what a closed window means; install it once with `App::on_window_closed`. It
/// forgets auxiliary windows that are gone, closes the rest when the main window is gone, and
/// returns true when no window is left (the caller quits).
pub fn window_closed(cx: &mut App) -> bool {
    let live = cx.windows();
    let main_gone = {
        let windows = cx.default_global::<AuxWindows>();
        windows.open.retain(|(handle, _)| live.contains(handle));
        windows.main.is_some_and(|main| !live.contains(&main))
    };
    if main_gone {
        close_all(cx);
    }
    cx.windows().is_empty()
}

/// The origin of the newest open window of `key`.
fn newest_origin(cx: &mut App, key: &'static str) -> Option<Point<Pixels>> {
    let live = cx.windows();
    let candidates: Vec<AnyWindowHandle> = cx
        .default_global::<AuxWindows>()
        .open
        .iter()
        .rev()
        .filter(|(handle, open_key)| *open_key == key && live.contains(handle))
        .map(|(handle, _)| *handle)
        .collect();
    candidates.into_iter().find_map(|handle| {
        handle
            .update(cx, |_, window, _| window.bounds().origin)
            .ok()
    })
}

// ---------------------------------------------------------------------------------------------
// Where a window opens. Pure, so it is tested without a window.

/// The corner of the window that opens after one at `origin`.
fn cascaded(origin: Point<Pixels>) -> Point<Pixels> {
    point(origin.x + CASCADE_OFFSET, origin.y + CASCADE_OFFSET)
}

/// `size` centred on `display`, shrunk to the display if it is bigger.
fn centered_on(size_wanted: Size<Pixels>, display: Bounds<Pixels>) -> Bounds<Pixels> {
    let width = f32::from(size_wanted.width).min(f32::from(display.size.width));
    let height = f32::from(size_wanted.height).min(f32::from(display.size.height));
    let centre = display.center();
    Bounds::new(
        point(
            px(f32::from(centre.x) - width / 2.0),
            px(f32::from(centre.y) - height / 2.0),
        ),
        size(px(width), px(height)),
    )
}

/// How far `a` and `b` overlap, as (width, height), zero when they do not.
fn overlap(a: Bounds<Pixels>, b: Bounds<Pixels>) -> (f32, f32) {
    let width = f32::from(a.right().min(b.right())) - f32::from(a.left().max(b.left()));
    let height = f32::from(a.bottom().min(b.bottom())) - f32::from(a.top().max(b.top()));
    (width.max(0.0), height.max(0.0))
}

/// Brings remembered bounds back to a display that exists. A window that no display overlaps by
/// at least a grabbable piece of its title bar is centred on the first display (the primary
/// one); any other is clamped fully inside the display it overlaps most, shrunk to the display
/// if it is bigger. With no display to go by, `saved` comes back unchanged.
pub fn fit_to_displays(saved: Bounds<Pixels>, displays: &[Bounds<Pixels>]) -> Bounds<Pixels> {
    let Some(&primary) = displays.first() else {
        return saved;
    };
    let need_width = f32::from(REACH_WIDTH).min(f32::from(saved.size.width));
    let need_height = f32::from(REACH_HEIGHT).min(f32::from(saved.size.height));
    let mut best: Option<(f32, Bounds<Pixels>)> = None;
    for &display in displays {
        let (width, height) = overlap(saved, display);
        if width < need_width || height < need_height {
            continue;
        }
        let area = width * height;
        if best.is_none_or(|(most, _)| area > most) {
            best = Some((area, display));
        }
    }
    let Some((_, display)) = best else {
        return centered_on(saved.size, primary);
    };
    let width = f32::from(saved.size.width).min(f32::from(display.size.width));
    let height = f32::from(saved.size.height).min(f32::from(display.size.height));
    let x = f32::from(saved.origin.x)
        .min(f32::from(display.right()) - width)
        .max(f32::from(display.left()));
    let y = f32::from(saved.origin.y)
        .min(f32::from(display.bottom()) - height)
        .max(f32::from(display.top()));
    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

/// Where a window opens: the remembered bounds (the size only, for a cascading kind), below
/// and right of the newest open window of a cascading kind, or centred; then back onto a
/// display.
fn initial_bounds(
    options: &AuxOptions,
    remembered: Option<Bounds<Pixels>>,
    newest: Option<Point<Pixels>>,
    displays: &[Bounds<Pixels>],
) -> Bounds<Pixels> {
    let wanted = remembered.map_or(options.size, |bounds| bounds.size);
    let window_size = wanted.max(&options.min_size);
    let centred = || match displays.first() {
        Some(&primary) => centered_on(window_size, primary),
        None => Bounds::new(Point::default(), window_size),
    };
    let bounds = if options.cascade {
        match newest {
            Some(origin) => Bounds::new(cascaded(origin), window_size),
            None => centred(),
        }
    } else {
        match remembered {
            Some(bounds) => Bounds::new(bounds.origin, window_size),
            None => centred(),
        }
    };
    fit_to_displays(bounds, displays)
}

// ---------------------------------------------------------------------------------------------
// The window.

/// Opens a window with our own title bar (drag area, title in 12 px `ink2`, close button),
/// `WindowKind::Normal`, a `chrome` bar over a `sheet` body. Restores the bounds remembered
/// under `options.bounds_key`, moved back onto a display that still exists, and remembers them
/// 500 ms after they stop changing and at close. Puts the `AuxWindow` key context on the root,
/// so Escape and Ctrl+W close it.
///
/// `build` makes the body. Its `focus_handle` takes the keyboard when the window opens, so it
/// must be tracked by an element of the body (the body's root, usually); the frame has a handle
/// of its own that a click on the bar focuses, which keeps Escape working there.
pub fn open<V: Render + Focusable>(
    options: AuxOptions,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> anyhow::Result<AnyWindowHandle> {
    let displays: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|display| display.bounds())
        .collect();
    let newest = if options.cascade {
        newest_origin(cx, options.bounds_key)
    } else {
        None
    };
    let bounds = initial_bounds(
        &options,
        remembered(cx, options.bounds_key),
        newest,
        &displays,
    );
    let (key, title) = (options.bounds_key, options.title);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.clone()),
            appears_transparent: true,
            traffic_light_position: Some(point(
                metrics::AUX_TRAFFIC_LIGHT_X,
                metrics::AUX_TRAFFIC_LIGHT_Y,
            )),
        }),
        focus: true,
        show: true,
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        // There is nothing to minimise to: the window belongs to the main one.
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        window_min_size: Some(options.min_size),
        ..Default::default()
    };
    let handle = cx.open_window(window_options, move |window, cx| {
        let body = build(window, cx);
        window.focus(&body.focus_handle(cx));
        cx.new(|cx| AuxFrame::new(body, key, title, window, cx))
    })?;
    let handle: AnyWindowHandle = handle.into();
    cx.default_global::<AuxWindows>().open.push((handle, key));
    Ok(handle)
}

/// The root view of an auxiliary window.
struct AuxFrame<V: 'static> {
    bounds_key: &'static str,
    title: SharedString,
    body: Entity<V>,
    focus: FocusHandle,
    /// The pending save of the bounds; a newer change replaces it.
    saving: Option<Task<()>>,
    _bounds: Subscription,
}

impl<V: Render + Focusable> AuxFrame<V> {
    fn new(
        body: Entity<V>,
        bounds_key: &'static str,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_window_bounds(window, |this, window, cx| {
            this.bounds_changed(window, cx);
        });
        // The system's own close (Alt+F4, the caption's close button) does not pass through
        // `close_window`.
        window.on_window_should_close(cx, move |window, cx| {
            remember(cx, bounds_key, window.window_bounds().get_bounds());
            true
        });
        Self {
            bounds_key,
            title,
            body,
            focus: cx.focus_handle(),
            saving: None,
            _bounds: subscription,
        }
    }

    /// The window moved or was resized: remembers where it is once it has stayed there for
    /// [`BOUNDS_SAVE_DELAY`].
    fn bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (key, bounds) = (self.bounds_key, window.window_bounds().get_bounds());
        self.saving = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(BOUNDS_SAVE_DELAY).await;
            cx.update(|cx| remember(cx, key, bounds)).ok();
        }));
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        close_window(window, cx);
    }
}

/// The close button. On Windows the control area makes Windows act on the click, so there is no
/// click handler; elsewhere `on_close` runs.
fn close_button(
    colors: Colors,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let button = div()
        .id("aux-close")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(metrics::CAPTION_BUTTON_WIDTH)
        .h(metrics::AUX_BAR_HEIGHT)
        // Without this the bar's drag layer, which comes first in paint order, would win the
        // hit test over the button.
        .occlude()
        .hover(|style| style.bg(colors.hover))
        .child(
            svg()
                .path(icon::CLOSE)
                .size(metrics::ICON_CAPTION)
                .text_color(colors.ink2),
        );
    if cfg!(target_os = "windows") {
        button.window_control_area(WindowControlArea::Close)
    } else {
        button.cursor_pointer().on_click(on_close)
    }
}

impl<V: Render + Focusable> Render for AuxFrame<V> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let is_windows = cfg!(target_os = "windows");
        let is_macos = cfg!(target_os = "macos");
        let left_padding = if is_macos {
            metrics::TRAFFIC_LIGHT_SPACE
        } else {
            metrics::ROW_PADDING
        };
        let bar = div()
            .id("aux-titlebar")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .relative()
            .w_full()
            .h(metrics::AUX_BAR_HEIGHT)
            .pl(left_padding)
            .bg(colors.chrome)
            // Windows turns this layer into the caption: it drags the window and a double
            // click maximizes it. The close button is marked `occlude` so it stays out of it.
            // The layer starts below the top resize edge unless the window is maximized.
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
            // macOS drags a transparent title bar natively; only the double-click is ours.
            .when(is_macos, |bar| {
                bar.on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            // Elsewhere the compositor moves the window.
            .when(!is_windows && !is_macos, |bar| {
                bar.on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink2)
                    .child(self.title.clone()),
            )
            // macOS has its traffic lights.
            .when(!is_macos, |bar| {
                bar.child(close_button(
                    colors,
                    cx.listener(|this, _: &ClickEvent, window, cx| this.close(window, cx)),
                ))
            });
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(AUX_WINDOW)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &keymap::aux::Close, window, cx| {
                this.close(window, cx);
            }))
            .bg(colors.chrome)
            .text_color(colors.ink)
            .font(fonts::ui_font())
            .text_size(text::BODY.size)
            .child(bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(colors.sheet)
                    .child(self.body.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};

    use super::*;

    fn display(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
        inner.left() >= outer.left()
            && inner.top() >= outer.top()
            && inner.right() <= outer.right()
            && inner.bottom() <= outer.bottom()
    }

    const LEFT: fn() -> Bounds<Pixels> = || display(0.0, 0.0, 1920.0, 1080.0);
    const RIGHT: fn() -> Bounds<Pixels> = || display(1920.0, 0.0, 1280.0, 1024.0);

    #[test]
    fn bounds_far_off_every_display_come_back_centred_on_the_first() {
        let saved = display(5000.0, -3000.0, 840.0, 560.0);
        let fitted = fit_to_displays(saved, &[LEFT(), RIGHT()]);
        assert_eq!(fitted, display(540.0, 260.0, 840.0, 560.0));
    }

    #[test]
    fn bounds_on_a_display_that_is_gone_come_back_onto_the_one_left() {
        let on_the_second = display(2200.0, 100.0, 840.0, 560.0);
        // While both displays exist the window stays where it was.
        assert_eq!(
            fit_to_displays(on_the_second, &[LEFT(), RIGHT()]),
            on_the_second
        );
        // With only the first left, it is on a display that exists.
        let fitted = fit_to_displays(on_the_second, &[LEFT()]);
        assert!(inside(fitted, LEFT()), "{fitted:?}");
        assert_eq!(fitted.size, on_the_second.size);
    }

    #[test]
    fn bounds_already_inside_a_display_are_unchanged() {
        let saved = display(100.0, 50.0, 600.0, 400.0);
        assert_eq!(fit_to_displays(saved, &[LEFT()]), saved);
    }

    #[test]
    fn bounds_partly_off_a_display_are_pulled_fully_inside() {
        let hanging = display(1500.0, 800.0, 840.0, 560.0);
        let fitted = fit_to_displays(hanging, &[LEFT()]);
        assert_eq!(fitted, display(1080.0, 520.0, 840.0, 560.0));
        let above = display(100.0, -40.0, 600.0, 400.0);
        assert_eq!(
            fit_to_displays(above, &[LEFT()]),
            display(100.0, 0.0, 600.0, 400.0)
        );
    }

    #[test]
    fn bounds_bigger_than_the_display_shrink_to_it() {
        let saved = display(-50.0, -50.0, 4000.0, 3000.0);
        assert_eq!(fit_to_displays(saved, &[LEFT()]), LEFT());
    }

    #[test]
    fn a_sliver_on_a_display_is_not_on_it() {
        // Ten pixels of the window's right edge touch the display: not meaningfully on it.
        let sliver = display(-830.0, 100.0, 840.0, 560.0);
        let fitted = fit_to_displays(sliver, &[LEFT()]);
        assert_eq!(fitted, display(540.0, 260.0, 840.0, 560.0));
    }

    #[test]
    fn no_display_leaves_the_bounds_alone() {
        let saved = display(5000.0, 5000.0, 100.0, 100.0);
        assert_eq!(fit_to_displays(saved, &[]), saved);
    }

    #[test]
    fn the_next_window_opens_24_px_down_and_right() {
        assert_eq!(
            cascaded(point(px(100.), px(200.))),
            point(px(124.), px(224.))
        );
    }

    fn options(cascade: bool) -> AuxOptions {
        AuxOptions {
            bounds_key: "window.settings",
            title: "Test".into(),
            size: size(px(400.), px(300.)),
            min_size: size(px(200.), px(150.)),
            cascade,
        }
    }

    #[test]
    fn a_cascading_window_follows_the_newest_one_and_keeps_only_the_size() {
        let remembered = display(900.0, 700.0, 500.0, 350.0);
        let bounds = initial_bounds(
            &options(true),
            Some(remembered),
            Some(point(px(300.), px(200.))),
            &[LEFT()],
        );
        assert_eq!(bounds, display(324.0, 224.0, 500.0, 350.0));
        // The first window is centred, whatever position was remembered.
        let first = initial_bounds(&options(true), Some(remembered), None, &[LEFT()]);
        assert_eq!(first, display(710.0, 365.0, 500.0, 350.0));
    }

    #[test]
    fn a_plain_window_reopens_where_it_was_and_never_under_its_minimum() {
        let saved = display(900.0, 700.0, 100.0, 100.0);
        let bounds = initial_bounds(&options(false), Some(saved), None, &[LEFT()]);
        assert_eq!(bounds, display(900.0, 700.0, 200.0, 150.0));
        let fresh = initial_bounds(&options(false), None, None, &[LEFT()]);
        assert_eq!(fresh, display(760.0, 390.0, 400.0, 300.0));
    }

    #[test]
    fn bounds_round_trip_through_their_text() {
        let bounds = display(10.0, -20.0, 840.0, 560.0);
        assert_eq!(bounds_text(bounds), "10,-20,840,560");
        assert_eq!(parse_bounds("10,-20,840,560"), Some(bounds));
        assert_eq!(
            parse_bounds(" 1 , 2 , 3 , 4 "),
            Some(display(1.0, 2.0, 3.0, 4.0))
        );
        // Fractions are rounded to whole pixels.
        assert_eq!(bounds_text(display(0.4, 0.6, 99.5, 100.4)), "0,1,100,100");
    }

    #[test]
    fn garbage_bounds_text_is_nothing() {
        for text in [
            "",
            "abc",
            "1,2,3",
            "1,2,3,4,5",
            "1,2,3,x",
            "1,2,0,4",
            "1,2,3,-4",
            "1.5,2,3,4",
            "1,2,99999999999999999999,4",
            "1,2,5000000,4",
        ] {
            assert_eq!(parse_bounds(text), None, "{text:?}");
        }
    }

    #[gpui::test]
    fn bounds_are_stored_in_the_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Settings::defaults());
            assert_eq!(remembered(cx, "window.settings"), None);
            let bounds = display(30.0, 40.0, 760.0, 560.0);
            remember(cx, "window.settings", bounds);
            assert_eq!(remembered(cx, "window.settings"), Some(bounds));
            assert_eq!(
                Settings::get(cx).text_of("window.settings"),
                Some("30,40,760,560")
            );
            // A bad line in the file reads as nothing remembered.
            let (bad, _) = Settings::parse(
                "window.quick_look = nonsense
",
            );
            cx.set_global(bad);
            assert_eq!(remembered(cx, "window.quick_look"), None);
        });
    }

    // The windows -------------------------------------------------------------------------

    /// A body with something to focus, as every real body has.
    struct Body {
        focus: FocusHandle,
    }

    impl Focusable for Body {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus.clone()
        }
    }

    impl Render for Body {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().track_focus(&self.focus)
        }
    }

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // Fonts may not load in a test; the palette is installed first.
            crate::theme::init(cx).ok();
            cx.set_global(Settings::defaults());
            keymap::init(cx);
            // The rule `main.rs` installs, minus the quit.
            cx.on_window_closed(|cx| {
                window_closed(cx);
            })
            .detach();
        });
    }

    fn open_aux(cx: &mut TestAppContext, cascade: bool) -> AnyWindowHandle {
        cx.update(|cx| {
            open(options(cascade), cx, |_, cx| {
                cx.new(|cx| Body {
                    focus: cx.focus_handle(),
                })
            })
            .expect("the window opens")
        })
    }

    fn origin_of(cx: &mut TestAppContext, handle: AnyWindowHandle) -> Point<Pixels> {
        cx.update(|cx| {
            handle
                .update(cx, |_, window, _| window.bounds().origin)
                .expect("the window is open")
        })
    }

    fn is_open(cx: &mut TestAppContext, handle: AnyWindowHandle) -> bool {
        cx.update(|cx| cx.windows().contains(&handle))
    }

    #[gpui::test]
    fn escape_closes_an_aux_window(cx: &mut TestAppContext) {
        setup(cx);
        let handle = open_aux(cx, false);
        assert!(is_open(cx, handle));
        cx.simulate_keystrokes(handle, "escape");
        assert!(!is_open(cx, handle));
    }

    #[gpui::test]
    fn ctrl_w_closes_an_aux_window(cx: &mut TestAppContext) {
        setup(cx);
        // "secondary" is Ctrl on this build's platform and Cmd on macOS, as the keymap says.
        let handle = open_aux(cx, false);
        cx.simulate_keystrokes(handle, "secondary-w");
        assert!(!is_open(cx, handle));
    }

    #[gpui::test]
    fn the_close_action_closes_an_aux_window(cx: &mut TestAppContext) {
        setup(cx);
        let handle = open_aux(cx, false);
        cx.dispatch_action(handle, keymap::aux::Close);
        assert!(!is_open(cx, handle));
    }

    #[gpui::test]
    fn closing_the_main_window_closes_every_aux_window(cx: &mut TestAppContext) {
        setup(cx);
        let main: WindowHandle<Empty> = cx.add_window(|_, _| Empty);
        cx.update(|cx| register_main(cx, main.into()));
        let first = open_aux(cx, false);
        let second = open_aux(cx, true);
        assert_eq!(cx.windows().len(), 3);
        cx.update(|cx| {
            main.update(cx, |_, window, _| window.remove_window())
                .expect("the main window is open");
        });
        cx.run_until_parked();
        assert!(cx.windows().is_empty(), "windows are still open");
        assert!(!is_open(cx, first) && !is_open(cx, second));
    }

    #[gpui::test]
    fn closing_an_aux_window_leaves_the_main_window_and_the_others(cx: &mut TestAppContext) {
        setup(cx);
        let main: WindowHandle<Empty> = cx.add_window(|_, _| Empty);
        cx.update(|cx| register_main(cx, main.into()));
        let first = open_aux(cx, false);
        let second = open_aux(cx, false);
        cx.simulate_keystrokes(first, "escape");
        assert!(!is_open(cx, first));
        assert!(is_open(cx, second));
        assert!(is_open(cx, main.into()));
    }

    #[gpui::test]
    fn a_cascading_window_opens_24_px_down_and_right_of_the_first(cx: &mut TestAppContext) {
        setup(cx);
        let first = open_aux(cx, true);
        let second = open_aux(cx, true);
        let third = open_aux(cx, true);
        let origin = origin_of(cx, first);
        assert_eq!(
            origin_of(cx, second),
            point(origin.x + px(24.), origin.y + px(24.))
        );
        assert_eq!(
            origin_of(cx, third),
            point(origin.x + px(48.), origin.y + px(48.))
        );
        // Closing the newest sends the next one back to the one before it.
        cx.simulate_keystrokes(third, "escape");
        let again = open_aux(cx, true);
        assert_eq!(
            origin_of(cx, again),
            point(origin.x + px(48.), origin.y + px(48.))
        );
    }

    #[gpui::test]
    fn the_bounds_are_remembered_after_they_stop_changing_and_at_close(cx: &mut TestAppContext) {
        setup(cx);
        let handle = open_aux(cx, false);
        let window = cx.update(|cx| {
            handle
                .update(cx, |_, window, _| window.window_bounds().get_bounds())
                .expect("the window is open")
        });
        assert_eq!(cx.update(|cx| remembered(cx, "window.settings")), None);
        // A resize is remembered once the delay has passed without another one.
        cx.simulate_window_resize(handle, size(px(700.), px(500.)));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| remembered(cx, "window.settings")), None);
        cx.executor().advance_clock(BOUNDS_SAVE_DELAY);
        cx.run_until_parked();
        let saved = cx
            .update(|cx| remembered(cx, "window.settings"))
            .expect("saved after the delay");
        assert_eq!(saved.size, size(px(700.), px(500.)));
        assert_ne!(saved.size, window.size);
        // The window reopens at the size it had.
        cx.simulate_keystrokes(handle, "escape");
        let reopened = open_aux(cx, false);
        let size_again = cx.update(|cx| {
            reopened
                .update(cx, |_, window, _| window.bounds().size)
                .expect("the window is open")
        });
        assert_eq!(size_again, size(px(700.), px(500.)));
    }
}
