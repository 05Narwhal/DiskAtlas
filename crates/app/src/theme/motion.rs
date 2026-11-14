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

//! Motion tokens (`r1-redesign.md` 2.1), the reduced-motion switch (2.3), and the small pure
//! pieces every animated component shares: [`Tween`] (eased progress from a start instant),
//! [`Disclosure`] (a container opening or closing), [`Entrance`] (a slide and fade in) and the
//! glide gate for the selection plate. Nothing here draws; components apply the values.
//!
//! GPUI 0.2.2 has no implicit transitions. A component either wraps an element in
//! `with_animation` (keyed by an id, restarting when the id changes) or keeps a [`Tween`] itself
//! and calls `window.request_animation_frame()` while it runs. The second suits anything that
//! must survive a frame where the element is not drawn, and every closing movement.

use std::time::{Duration, Instant};

use gpui::{Animation, App, Global};

/// How long Quick Look's window fades in. Not played when reduced motion is on.
pub const QUICK_LOOK_FADE: Duration = Duration::from_millis(120);

/// How long the pointer rests on a menu's submenu row before the submenu opens.
pub const SUBMENU_REST: Duration = Duration::from_millis(150);

/// One pass of the measuring sweep over a storage bar (`r1-redesign.md` 2: 1.2 s). Reduced
/// motion draws a static stripe.
pub const SWEEP_PASS: Duration = Duration::from_millis(1200);

/// The environment variable that turns reduced motion on.
pub const REDUCED_MOTION_VAR: &str = "DISK_ATLAS_REDUCED_MOTION";

/// The shape of an animation's progress over its duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Easing {
    /// No animation: the end state at once.
    None,
    /// Ease-out cubic, `1 - (1 - t)^3`.
    OutCubic,
    /// Ease-out quint, `1 - (1 - t)^5` (the same curve as `gpui::ease_out_quint`).
    OutQuint,
    /// Ease-in cubic, `t^3`, for every closing movement.
    InCubic,
}

impl Easing {
    /// Progress for `t` in `0.0..=1.0` (values outside are clamped).
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::None => 1.0,
            Easing::OutCubic => 1.0 - (1.0 - t).powi(3),
            Easing::OutQuint => 1.0 - (1.0 - t).powi(5),
            Easing::InCubic => t * t * t,
        }
    }
}

/// One timing token: how long, and how it eases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Length of the movement.
    pub duration: Duration,
    /// Its easing.
    pub easing: Easing,
}

impl Timing {
    /// The closing movement for this entry timing: 0.7 times as long, eased in with a cubic.
    pub const fn exit(self) -> Timing {
        Timing {
            duration: Duration::from_micros(self.duration.as_micros() as u64 * 7 / 10),
            easing: if self.duration.is_zero() {
                Easing::None
            } else {
                Easing::InCubic
            },
        }
    }

    /// A GPUI animation with this timing, for `AnimationExt::with_animation`. Do not build one
    /// for [`INSTANT`]: GPUI divides by the duration; draw the end state instead.
    pub fn animation(self) -> Animation {
        let easing = self.easing;
        Animation::new(self.duration).with_easing(move |t| easing.apply(t))
    }
}

/// Selection under key repeat, typing, results arriving.
pub const INSTANT: Timing = Timing {
    duration: Duration::ZERO,
    easing: Easing::None,
};
/// Hover fade, button press, one-step selection glide, crossfade between Quick Look files.
pub const QUICK: Timing = Timing {
    duration: Duration::from_millis(90),
    easing: Easing::OutCubic,
};
/// Details pane, sidebar groups, folder navigation.
pub const STANDARD: Timing = Timing {
    duration: Duration::from_millis(160),
    easing: Easing::OutQuint,
};
/// Quick Look content opening, space-map zoom.
pub const EMPHASIZED: Timing = Timing {
    duration: Duration::from_millis(240),
    easing: Easing::OutQuint,
};

/// The reduced-motion setting, three positions (2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReducedMotion {
    /// Follow the system's setting (and the environment variable).
    #[default]
    System,
    /// Always reduce.
    On,
    /// Never reduce.
    Off,
}

impl ReducedMotion {
    /// Whether motion is reduced, given what the system reports. `System` also honours the
    /// environment variable, which keeps working.
    pub fn is_reduced(self, system_reduced: bool) -> bool {
        match self {
            ReducedMotion::On => true,
            ReducedMotion::Off => false,
            ReducedMotion::System => system_reduced || env_flag(),
        }
    }
}

/// True for a value of [`REDUCED_MOTION_VAR`] that asks for no animation.
pub fn flag_is_on(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| {
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

/// The environment variable's current answer.
pub fn env_flag() -> bool {
    flag_is_on(std::env::var(REDUCED_MOTION_VAR).ok().as_deref())
}

/// Whether motion is reduced, from the three inputs in order: the environment variable, when it
/// is set to something (an on value reduces, `0` or `false` forces full motion); else the
/// `motion` setting (`reduced`, `full`); `system` falls back to what the system reports.
pub fn resolve_reduced(env_value: Option<&str>, setting: &str, system_reduced: bool) -> bool {
    if env_value.is_some_and(|value| !value.trim().is_empty()) {
        return flag_is_on(env_value);
    }
    match setting {
        "reduced" => true,
        "full" => false,
        _ => system_reduced,
    }
}

/// What the system and the environment said about motion, installed as a global by [`init`] and
/// kept current by [`refresh_system`].
#[derive(Debug, Clone, Default)]
pub struct MotionInputs {
    /// The value of [`REDUCED_MOTION_VAR`] at startup, if set.
    env_value: Option<String>,
    /// True when the operating system asks for less motion. False until it has answered, and
    /// where it cannot be asked.
    system_reduced: bool,
}

impl Global for MotionInputs {}

/// Reads the environment variable once and asks the system (off the UI thread). Call at startup,
/// before the first window.
pub fn init(cx: &mut App) {
    cx.set_global(MotionInputs {
        env_value: std::env::var(REDUCED_MOTION_VAR).ok(),
        system_reduced: false,
    });
    refresh_system(cx);
}

/// Asks the operating system again whether it wants less motion, on the background executor, and
/// repaints every window when the answer changed. Call when a window regains focus.
pub fn refresh_system(cx: &mut App) {
    let query = cx
        .background_executor()
        .spawn(async { crate::platform::reduced_motion::system_reduces_motion() });
    cx.spawn(async move |cx| {
        let Some(reduced) = query.await else {
            return;
        };
        cx.update(|cx| {
            let inputs = cx.default_global::<MotionInputs>();
            if inputs.system_reduced != reduced {
                inputs.system_reduced = reduced;
                tracing::debug!(reduced, "the system's reduced-motion setting changed");
                cx.refresh_windows();
            }
        })
        .ok();
    })
    .detach();
}

/// Whether motion is reduced now: the environment variable, else the `motion` setting, else the
/// system (see [`resolve_reduced`]). Cheap enough to call on every render.
pub fn reduced(cx: &App) -> bool {
    let setting = cx
        .try_global::<crate::state::settings::Settings>()
        .map_or("system", |settings| settings.choice_of("motion"));
    match cx.try_global::<MotionInputs>() {
        Some(inputs) => {
            resolve_reduced(inputs.env_value.as_deref(), setting, inputs.system_reduced)
        }
        // No global (unit tests): read the variable directly.
        None => resolve_reduced(
            std::env::var(REDUCED_MOTION_VAR).ok().as_deref(),
            setting,
            false,
        ),
    }
}

/// What a movement becomes when motion is reduced: slides and scales turn into a fade over
/// `QUICK`; a movement that is already instant stays instant.
pub const fn reduce(timing: Timing) -> Timing {
    if timing.duration.is_zero() {
        INSTANT
    } else {
        QUICK
    }
}

/// `timing`, or its reduced form when `reduced`.
pub const fn timing_for(timing: Timing, reduced: bool) -> Timing {
    if reduced { reduce(timing) } else { timing }
}

/// Linear interpolation from `from` to `to` by `t` (not clamped).
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// A movement in progress: when it started and how it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tween {
    /// When the movement started.
    pub start: Instant,
    /// How long it runs and how it eases.
    pub timing: Timing,
}

impl Tween {
    /// A movement with `timing` starting at `start`.
    pub const fn new(start: Instant, timing: Timing) -> Self {
        Self { start, timing }
    }

    /// The eased progress at `now`, or `None` once the movement has finished (at once for an
    /// instant timing). A `now` before the start counts as the start.
    pub fn at(&self, now: Instant) -> Option<f32> {
        let total = self.timing.duration.as_secs_f32();
        if total <= 0.0 {
            return None;
        }
        let t = now.saturating_duration_since(self.start).as_secs_f32() / total;
        (t < 1.0).then(|| self.timing.easing.apply(t))
    }
}

/// How a disclosed container is drawn in one frame of a [`Disclosure`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reveal {
    /// The fraction of its full height (or width) to give the container, `0.0..=1.0`.
    pub extent: f32,
    /// The opacity of its content, `0.0..=1.0`.
    pub opacity: f32,
}

/// A container opening or closing (sidebar groups, the details pane, 2.2). Full motion grows or
/// shrinks its size over `STANDARD` (closing over the exit timing); reduced motion keeps the size
/// and fades over `QUICK` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disclosure {
    /// True while opening, false while closing.
    pub opening: bool,
    /// Whether the reduced form was chosen when it started.
    pub reduced: bool,
    tween: Tween,
}

impl Disclosure {
    /// Starts opening (`opening`) or closing at `now`.
    pub fn new(opening: bool, now: Instant, reduced: bool) -> Self {
        let entry = timing_for(STANDARD, reduced);
        let timing = if opening { entry } else { entry.exit() };
        Self {
            opening,
            reduced,
            tween: Tween::new(now, timing),
        }
    }

    /// The frame at `now`, or `None` once the movement has finished: then an opened container is
    /// drawn as it is and a closed one not at all.
    pub fn at(&self, now: Instant) -> Option<Reveal> {
        let t = self.tween.at(now)?;
        let shown = if self.opening { t } else { 1.0 - t };
        Some(if self.reduced {
            Reveal {
                extent: 1.0,
                opacity: shown,
            }
        } else {
            Reveal {
                extent: shown,
                opacity: 1.0,
            }
        })
    }
}

/// New content sliding in from one side and fading in (folder navigation, 2.2). Reduced motion
/// drops the slide and fades over `QUICK`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entrance {
    /// Where the content starts, in pixels from its resting place (negative: from the left).
    pub offset: f32,
    tween: Tween,
}

impl Entrance {
    /// Starts at `now`: from `offset` pixels away over `timing`, or as a fade over its reduced
    /// form when `reduced`.
    pub fn new(offset: f32, timing: Timing, now: Instant, reduced: bool) -> Self {
        Self {
            offset: if reduced { 0.0 } else { offset },
            tween: Tween::new(now, timing_for(timing, reduced)),
        }
    }

    /// `(offset, opacity)` at `now`, or `None` once the content is at rest.
    pub fn at(&self, now: Instant) -> Option<(f32, f32)> {
        let t = self.tween.at(now)?;
        Some((self.offset * (1.0 - t), t))
    }
}

/// How far folder navigation slides the new listing (2.2).
pub const FOLDER_SLIDE: f32 = 12.0;

/// Two keyboard moves closer together than this are a held key (or fast typing): the second
/// does not glide. Twice `QUICK`, so a glide always finishes before the next one starts.
pub const GLIDE_REPEAT_GAP: Duration = Duration::from_micros(QUICK.duration.as_micros() as u64 * 2);

/// Whether a one-step keyboard move made at `now` glides the selection plate (2.2): never under
/// reduced motion, and not when the previous move was less than [`GLIDE_REPEAT_GAP`] before.
pub fn glides(previous_move: Option<Instant>, now: Instant, reduced: bool) -> bool {
    !reduced
        && previous_move
            .is_none_or(|previous| now.saturating_duration_since(previous) >= GLIDE_REPEAT_GAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_match_the_table() {
        assert_eq!(INSTANT.duration, Duration::ZERO);
        assert_eq!(QUICK.duration, Duration::from_millis(90));
        assert_eq!(STANDARD.duration, Duration::from_millis(160));
        assert_eq!(EMPHASIZED.duration, Duration::from_millis(240));
    }

    #[test]
    fn exit_is_seven_tenths_and_eased_in() {
        assert_eq!(STANDARD.exit().duration, Duration::from_millis(112));
        assert_eq!(EMPHASIZED.exit().duration, Duration::from_millis(168));
        assert_eq!(QUICK.exit().easing, Easing::InCubic);
        assert_eq!(INSTANT.exit(), INSTANT);
    }

    #[test]
    fn easings_run_from_zero_to_one() {
        for easing in [Easing::OutCubic, Easing::OutQuint, Easing::InCubic] {
            assert_eq!(easing.apply(0.0), 0.0);
            assert_eq!(easing.apply(1.0), 1.0);
            assert_eq!(easing.apply(-3.0), 0.0);
            assert_eq!(easing.apply(7.0), 1.0);
        }
        assert_eq!(Easing::None.apply(0.0), 1.0);
        assert!(Easing::OutCubic.apply(0.5) > 0.5);
        assert!(Easing::InCubic.apply(0.5) < 0.5);
    }

    #[test]
    fn out_quint_matches_gpui() {
        let gpui_quint = gpui::ease_out_quint();
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            assert!((Easing::OutQuint.apply(t) - gpui_quint(t)).abs() < 1e-6);
        }
    }

    #[test]
    fn reduced_motion_resolution() {
        assert!(ReducedMotion::On.is_reduced(false));
        assert!(!ReducedMotion::Off.is_reduced(true));
        assert!(ReducedMotion::System.is_reduced(true));
    }

    #[test]
    fn motion_setting_resolution() {
        // The environment variable overrides the setting, both ways.
        assert!(resolve_reduced(Some("1"), "full", false));
        assert!(!resolve_reduced(Some("0"), "reduced", true));
        // Unset or empty: the setting decides, and `system` follows the system.
        assert!(resolve_reduced(None, "reduced", false));
        assert!(!resolve_reduced(Some(""), "full", true));
        assert!(resolve_reduced(None, "system", true));
        assert!(!resolve_reduced(None, "system", false));
    }

    #[test]
    fn flag_values() {
        assert!(!flag_is_on(None));
        assert!(!flag_is_on(Some("")));
        assert!(!flag_is_on(Some("0")));
        assert!(!flag_is_on(Some("False")));
        assert!(flag_is_on(Some("1")));
        assert!(flag_is_on(Some(" yes ")));
    }

    #[test]
    fn reduced_movements_fade_over_quick() {
        assert_eq!(reduce(EMPHASIZED), QUICK);
        assert_eq!(reduce(STANDARD), QUICK);
        assert_eq!(reduce(INSTANT), INSTANT);
        assert_eq!(timing_for(STANDARD, false), STANDARD);
        assert_eq!(timing_for(STANDARD, true), QUICK);
    }

    fn ms(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn lerp_runs_between_the_ends() {
        assert_eq!(lerp(10.0, 30.0, 0.0), 10.0);
        assert_eq!(lerp(10.0, 30.0, 0.5), 20.0);
        assert_eq!(lerp(30.0, 10.0, 1.0), 10.0);
    }

    #[test]
    fn a_tween_eases_and_ends() {
        let start = Instant::now();
        let tween = Tween::new(start, STANDARD);
        assert_eq!(tween.at(start), Some(0.0));
        let mid = tween.at(ms(start, 80)).unwrap();
        assert!((mid - Easing::OutQuint.apply(0.5)).abs() < 1e-3);
        assert_eq!(tween.at(ms(start, 160)), None);
        assert_eq!(tween.at(ms(start, 10_000)), None);
        // A clock read before the start counts as the start.
        let early = Tween::new(ms(start, 50), QUICK);
        assert_eq!(early.at(start), Some(0.0));
        // An instant movement is over at once, and never divides by zero.
        assert_eq!(Tween::new(start, INSTANT).at(start), None);
    }

    #[test]
    fn a_disclosure_grows_then_rests() {
        let start = Instant::now();
        let open = Disclosure::new(true, start, false);
        assert_eq!(
            open.at(start),
            Some(Reveal {
                extent: 0.0,
                opacity: 1.0
            })
        );
        let half = open.at(ms(start, 80)).unwrap();
        assert!(half.extent > 0.5 && half.extent < 1.0);
        assert_eq!(half.opacity, 1.0);
        assert_eq!(open.at(ms(start, 160)), None);
    }

    #[test]
    fn a_closing_disclosure_shrinks_over_the_exit_timing() {
        let start = Instant::now();
        let close = Disclosure::new(false, start, false);
        assert_eq!(close.at(start).unwrap().extent, 1.0);
        let late = close.at(ms(start, 100)).unwrap();
        assert!(late.extent < 0.5, "ease-in closes slowly, then fast");
        assert_eq!(close.at(ms(start, 112)), None);
    }

    #[test]
    fn a_reduced_disclosure_fades_at_full_size() {
        let start = Instant::now();
        let open = Disclosure::new(true, start, true);
        let frame = open.at(ms(start, 45)).unwrap();
        assert_eq!(frame.extent, 1.0);
        assert!(frame.opacity > 0.0 && frame.opacity < 1.0);
        assert_eq!(open.at(ms(start, 90)), None);
        let close = Disclosure::new(false, start, true);
        assert_eq!(close.at(start).unwrap().opacity, 1.0);
        assert_eq!(close.at(start).unwrap().extent, 1.0);
        assert_eq!(close.at(ms(start, 63)), None);
    }

    #[test]
    fn an_entrance_slides_and_fades_in() {
        let start = Instant::now();
        let entrance = Entrance::new(FOLDER_SLIDE, STANDARD, start, false);
        assert_eq!(entrance.at(start), Some((FOLDER_SLIDE, 0.0)));
        let (offset, opacity) = entrance.at(ms(start, 40)).unwrap();
        assert!(offset > 0.0 && offset < FOLDER_SLIDE);
        assert!(opacity > 0.0 && opacity < 1.0);
        assert_eq!(entrance.at(ms(start, 160)), None);
        // From the left.
        let back = Entrance::new(-FOLDER_SLIDE, STANDARD, start, false);
        assert!(back.at(ms(start, 40)).unwrap().0 < 0.0);
    }

    #[test]
    fn a_reduced_entrance_only_fades_over_quick() {
        let start = Instant::now();
        let entrance = Entrance::new(FOLDER_SLIDE, STANDARD, start, true);
        let (offset, opacity) = entrance.at(ms(start, 45)).unwrap();
        assert_eq!(offset, 0.0);
        assert!(opacity > 0.0 && opacity < 1.0);
        assert_eq!(entrance.at(ms(start, 90)), None);
    }

    #[test]
    fn held_keys_and_reduced_motion_do_not_glide() {
        let start = Instant::now();
        assert_eq!(GLIDE_REPEAT_GAP, Duration::from_millis(180));
        assert!(glides(None, start, false));
        assert!(!glides(None, start, true));
        // A key repeating every 33 ms.
        assert!(!glides(Some(start), ms(start, 33), false));
        // Two deliberate presses.
        assert!(glides(Some(start), ms(start, 180), false));
        assert!(glides(Some(start), ms(start, 400), false));
        assert!(!glides(Some(start), ms(start, 400), true));
    }
}
