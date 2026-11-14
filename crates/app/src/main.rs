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

mod app;
mod assets;
mod components;
mod elevation;
mod format;
mod frame_trace;
mod keymap;
mod platform;
mod preview;
mod state;
mod theme;
mod views;

use gpui::{
    App, AppContext, Application, Bounds, Pixels, Size, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowOptions, point, px, size,
};
use tracing_subscriber::EnvFilter;

use crate::app::Workspace;

/// Window title.
const WINDOW_TITLE: &str = "Disk Atlas";
/// Initial window size, in pixels.
const WINDOW_WIDTH: f32 = 1180.0;
const WINDOW_HEIGHT: f32 = 760.0;
/// Smallest size the window can be resized to.
const WINDOW_MIN_WIDTH: f32 = 760.0;
const WINDOW_MIN_HEIGHT: f32 = 440.0;
/// Where macOS places the traffic lights inside the app-drawn top bar.
const TRAFFIC_LIGHT_X: f32 = 14.0;
const TRAFFIC_LIGHT_Y: f32 = 15.0;

fn main() {
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            let filter =
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
            // A subscriber may already be installed; that is fine.
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .try_init()
                .ok();

            // One small synchronous read, before the first window opens.
            cx.set_global(state::settings::Settings::load());
            // Reduced motion: the environment variable now, the system's answer shortly after.
            theme::motion::init(cx);
            frame_trace::init(cx);

            if let Err(error) = theme::init(cx) {
                tracing::error!(%error, "failed to initialise the theme");
            }
            keymap::init(cx);

            // Closing the main window closes every auxiliary window; quit when the last window
            // closes.
            cx.on_window_closed(|cx| {
                if views::aux_window::window_closed(cx) {
                    cx.quit();
                }
            })
            .detach();
            // A settings save still in flight is written before the process ends.
            cx.on_app_quit(|_| async { state::settings::Settings::flush() })
                .detach();

            let bounds: Bounds<Pixels> =
                Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
            let min_size: Size<Pixels> = size(px(WINDOW_MIN_WIDTH), px(WINDOW_MIN_HEIGHT));

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(WINDOW_TITLE.into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(TRAFFIC_LIGHT_X), px(TRAFFIC_LIGHT_Y))),
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(min_size),
                window_background: WindowBackgroundAppearance::Opaque,
                ..Default::default()
            };

            match cx.open_window(options, |window, cx| {
                cx.new(|cx| Workspace::new(window, cx))
            }) {
                Ok(main) => {
                    views::aux_window::register_main(cx, main.into());
                    // Debug builds: DISK_ATLAS_OPEN_SETTINGS=<section>[:<search>] opens Settings.
                    views::settings::open_from_env(cx);
                }
                Err(error) => {
                    tracing::error!(%error, "failed to open the Disk Atlas window");
                    cx.quit();
                }
            }
        });
}
