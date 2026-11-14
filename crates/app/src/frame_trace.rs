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

//! Frame timings for the motion budget (`r1-redesign.md` 2.4, `performance.md`).
//!
//! Off unless `DISK_ATLAS_FRAME_TRACE` is set. Then every frame logs one `frame` line at info
//! level: the time from the start of the root view's render to the end of the last paint
//! (`cpu_ms`: render, layout, prepaint and paint on the UI thread; not the GPU's work), the time
//! since the previous frame started (`interval_ms`), and the animations that asked for the
//! frame. While something animates, frames follow one another at the display's refresh
//! interval, so an `interval_ms` well over it is a dropped frame.

use std::time::Instant;

use gpui::{App, Global, IntoElement, Styled, canvas};

/// The environment variable that turns the trace on.
pub const FRAME_TRACE_VAR: &str = "DISK_ATLAS_FRAME_TRACE";

/// Most labels one frame keeps; one per animated component is plenty.
const MAX_LABELS: usize = 8;

#[derive(Default)]
struct FrameTrace {
    started: Option<Instant>,
    previous_start: Option<Instant>,
    labels: Vec<&'static str>,
}

impl Global for FrameTrace {}

/// Installs the trace when [`FRAME_TRACE_VAR`] is set to something other than `0`.
pub fn init(cx: &mut App) {
    let on = std::env::var(FRAME_TRACE_VAR).is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0"
    });
    if on {
        tracing::info!("frame trace on");
        cx.set_global(FrameTrace::default());
    }
}

/// Marks the start of a frame. Call first thing in the root view's render.
pub fn begin(cx: &mut App) {
    if cx.try_global::<FrameTrace>().is_none() {
        return;
    }
    let trace = cx.global_mut::<FrameTrace>();
    trace.previous_start = trace.started.replace(Instant::now());
    trace.labels.clear();
}

/// Names an animation that asked for this frame. Free when the trace is off.
pub fn note(cx: &mut App, label: &'static str) {
    if cx.try_global::<FrameTrace>().is_none() {
        return;
    }
    let trace = cx.global_mut::<FrameTrace>();
    if trace.labels.len() < MAX_LABELS && !trace.labels.contains(&label) {
        trace.labels.push(label);
    }
}

/// An empty element that ends the frame's timing when it paints. Make it the root view's last
/// child, so it paints after everything else.
pub fn end_marker() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |_, (), _, cx| {
            if cx.try_global::<FrameTrace>().is_none() {
                return;
            }
            let trace = cx.global_mut::<FrameTrace>();
            let Some(started) = trace.started else {
                return;
            };
            let cpu_ms = started.elapsed().as_secs_f64() * 1000.0;
            let interval_ms = trace
                .previous_start
                .map(|previous| started.saturating_duration_since(previous).as_secs_f64() * 1000.0);
            tracing::info!(
                target: "disk_atlas::frame",
                cpu_ms = format_args!("{cpu_ms:.3}"),
                interval_ms = format_args!("{:.3}", interval_ms.unwrap_or(0.0)),
                labels = %trace.labels.join(","),
                "frame"
            );
        },
    )
    .absolute()
    .size_0()
}
