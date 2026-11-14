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

//! The Settings window (`r10-everyday.md` 8.2 and 8.3), in an auxiliary window of its own.
//!
//! Every row is drawn from the [`SETTINGS`] table: a switch for a yes or no, a segmented control
//! for two to four choices, a menu for more, a stepper for a number. A change goes to
//! [`Settings::set`] at once; there is no OK or Cancel. The left column holds the "Find a
//! setting" field and the sections; typing filters the rows of every section by label, help and
//! option names, and the section list then shows only the sections with matches and how many.
//!
//! The row logic ([`matches`], [`rows_of`], [`sections_with_matches`], [`stepped_int`]) is pure
//! and tested without a window. Nothing here touches a disk; the settings save in the
//! background.
//!
//! Sections the table does not fill: Keyboard (a static list of the shortcuts the keymap binds,
//! kept by hand next to `keymap.rs`), Advanced's actions, and About. Extensions appears with M8.

use std::cell::Cell;
use std::collections::HashMap;
use std::f32::consts::FRAC_PI_2;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context, Div, FocusHandle,
    Focusable, Global, IntoElement, KeyDownEvent, Pixels, Render, SharedString, Stateful,
    Subscription, Transformation, WeakEntity, Window, canvas, div, point, prelude::*, px, radians,
    size, svg, transparent_black,
};

use crate::assets::icon;
use crate::components::menu::{CloseReason, MenuItem, MenuSlot};
use crate::keymap::workspace::{FocusNext, FocusPrev};
use crate::keymap::{BUTTON, SETTINGS_SECTIONS, SETTINGS_SEGMENTS, SETTINGS_WINDOW};
use crate::keymap::{controls, settings as keys};
use crate::state::settings::{SETTINGS, Section, SettingDef, SettingKind, SettingValue, Settings};
use crate::theme::{
    Colors, Glyph, Theme, metrics,
    scale::{radius, size as sizes, space, text},
};
use crate::views::aux_window::{self, AuxOptions};

/// The key the window's bounds are saved under.
const BOUNDS_KEY: &str = "window.settings";
/// The environment variable that opens the window on a section at startup (debug builds only):
/// `<section key>[:<search text>]`.
pub const OPEN_ENV: &str = "DISK_ATLAS_OPEN_SETTINGS";

/// Size the window first opens at.
const WINDOW_SIZE: (f32, f32) = (760., 560.);
/// Smallest size of the window.
const MIN_SIZE: (f32, f32) = (640., 420.);
/// Width of the left column.
const LIST_WIDTH: Pixels = px(200.);
/// Height of one row of settings.
const ROW_HEIGHT: Pixels = px(56.);
/// Width of the switch's track.
const SWITCH_WIDTH: Pixels = px(40.);
/// Height of the switch's track.
const SWITCH_HEIGHT: Pixels = px(22.);
/// Side of the switch's thumb.
const SWITCH_THUMB: Pixels = px(16.);
/// Side of the glyph before a section's name.
const SECTION_GLYPH: Pixels = px(16.);
/// Most bytes the search text holds.
const MAX_QUERY_BYTES: usize = 100;
/// How many steps a stepper takes to cross its range, roughly.
const STEPS_ACROSS: u64 = 32;

// ---------------------------------------------------------------------------------------------
// What the sections hold. Pure.

/// The sections in the window, in order. Extensions is not here: it appears with M8.
pub const SECTIONS: [Section; 10] = [
    Section::General,
    Section::Appearance,
    Section::Browsing,
    Section::FolderSizes,
    Section::Disks,
    Section::QuickLook,
    Section::Copying,
    Section::Keyboard,
    Section::Advanced,
    Section::About,
];

/// A section's name.
pub const fn section_name(section: Section) -> &'static str {
    match section {
        Section::General => "General",
        Section::Appearance => "Appearance",
        Section::Browsing => "Browsing",
        Section::FolderSizes => "Folder sizes",
        Section::Disks => "Disks",
        Section::QuickLook => "Quick Look",
        Section::Copying => "Copying",
        Section::Keyboard => "Keyboard",
        Section::Extensions => "Extensions",
        Section::Advanced => "Advanced",
        Section::About => "About",
    }
}

/// The word that names a section in `DISK_ATLAS_OPEN_SETTINGS`.
pub const fn section_key(section: Section) -> &'static str {
    match section {
        Section::General => "general",
        Section::Appearance => "appearance",
        Section::Browsing => "browsing",
        Section::FolderSizes => "folder_sizes",
        Section::Disks => "disks",
        Section::QuickLook => "quick_look",
        Section::Copying => "copying",
        Section::Keyboard => "keyboard",
        Section::Extensions => "extensions",
        Section::Advanced => "advanced",
        Section::About => "about",
    }
}

/// The section a key word names.
pub fn section_from_key(word: &str) -> Option<Section> {
    let word = word.trim().to_lowercase();
    SECTIONS
        .iter()
        .copied()
        .find(|section| section_key(*section) == word)
}

/// The glyph before a section's name. The legend's kind glyphs are the only ones there are
/// today; they are drawn in `ink2`, not in their tints.
const fn section_glyph(section: Section) -> Glyph {
    match section {
        Section::General => Glyph::Application,
        Section::Appearance => Glyph::Picture,
        Section::Browsing => Glyph::Folder,
        Section::FolderSizes => Glyph::Archive,
        Section::Disks => Glyph::DiskImage,
        Section::QuickLook => Glyph::Movie,
        Section::Copying => Glyph::Document,
        Section::Keyboard => Glyph::Code,
        Section::Extensions | Section::Advanced => Glyph::Font,
        Section::About => Glyph::Other,
    }
}

/// One shortcut of the Keyboard section.
struct Shortcut {
    /// The keys, comma separated; "Ctrl" reads "Cmd" on macOS.
    keys: &'static str,
    action: &'static str,
}

/// A group of shortcuts.
struct ShortcutGroup {
    name: &'static str,
    shortcuts: &'static [Shortcut],
}

const fn shortcut(keys: &'static str, action: &'static str) -> Shortcut {
    Shortcut { keys, action }
}

/// The shortcuts `keymap.rs` binds, grouped as `ui-design.md` groups them. Kept by hand: the
/// keymap is a list of GPUI bindings without names a person can read.
static KEYBOARD: &[ShortcutGroup] = &[
    ShortcutGroup {
        name: "Moving around",
        shortcuts: &[
            shortcut(
                "Up, Down, Home, End, Page Up, Page Down",
                "Move the selection",
            ),
            shortcut("Enter", "Open the folder under the cursor"),
            shortcut("Backspace, Alt+Up", "Go to the enclosing folder"),
            shortcut("Alt+Left, Alt+Right", "Go back, go forward"),
            shortcut("Shift+Enter", "Open a file of this computer in its app"),
            shortcut("F5", "Read this folder again"),
        ],
    },
    ShortcutGroup {
        name: "Searching",
        shortcuts: &[
            shortcut("Ctrl+F", "Search this volume"),
            shortcut(
                "Down, Enter",
                "From the search field, go to the first result",
            ),
            shortcut("Ctrl+Enter", "Show a search result in its folder"),
            shortcut("Escape", "Leave the search"),
        ],
    },
    ShortcutGroup {
        name: "Quick Look",
        shortcuts: &[
            shortcut("Space", "Open or close Quick Look"),
            shortcut("Ctrl+Tab, Ctrl+Shift+Tab", "Next tab, previous tab"),
            shortcut("Ctrl+G", "Go to a byte"),
        ],
    },
    ShortcutGroup {
        name: "Window",
        shortcuts: &[
            shortcut("Ctrl+I", "Show or hide the details pane"),
            shortcut("Ctrl+Shift+.", "Show or hide hidden items"),
            shortcut("Ctrl+O", "Open a disk image"),
            shortcut("Ctrl+Shift+O", "Open a disk image that is a folder"),
            shortcut(
                "Menu key, Shift+F10",
                "Open the menu of the item under the cursor",
            ),
            shortcut("Ctrl+,", "Open Settings"),
            shortcut("Escape, Ctrl+W", "Close Settings or Quick Look"),
        ],
    },
];

/// One of the buttons of the Advanced section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    OpenSettingsFile,
    OpenCacheFolder,
    ResetAll,
}

/// An action of the Advanced section: its label, what it does, and its button.
struct ActionDef {
    act: Act,
    label: &'static str,
    help: &'static str,
    button: &'static str,
}

const ACTIONS: [ActionDef; 3] = [
    ActionDef {
        act: Act::OpenSettingsFile,
        label: "Open the settings file",
        help: "Show the plain-text file these settings are kept in",
        button: "Open",
    },
    ActionDef {
        act: Act::OpenCacheFolder,
        label: "Open the cache folder",
        help: "Show the folder where saved search indexes are kept",
        button: "Open",
    },
    ActionDef {
        act: Act::ResetAll,
        label: "Reset every setting\u{2026}",
        help: "Put every setting in this window back to its default. Saved places and window positions stay",
        button: "Reset\u{2026}",
    },
];

/// The lines of the About section.
fn about_lines() -> [String; 4] {
    [
        format!("Disk Atlas, version {}", env!("CARGO_PKG_VERSION")),
        "Free software, GPL-3.0-or-later.".to_owned(),
        "The text is set in Atkinson Hyperlegible Next, under the SIL Open Font License."
            .to_owned(),
        "Disk Atlas never writes to a disk it reads itself.".to_owned(),
    ]
}

/// True when every word of `query` is somewhere in `haystack`, whatever the case.
fn contains_all(haystack: &str, query: &str) -> bool {
    let haystack = haystack.to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

/// True for a setting the window shows on this platform.
fn is_shown(def: &SettingDef) -> bool {
    !matches!(def.kind, SettingKind::Hidden)
        && def.platforms.here()
        && def.section != Section::Extensions
}

/// True when `def` answers `query`: every word of it is in the label, the help line or an option
/// name. An empty query matches everything.
pub fn matches(def: &SettingDef, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let mut haystack = format!("{} {}", def.label, def.help);
    if let SettingKind::Choice { options, .. } = def.kind {
        for (_, label) in options {
            haystack.push(' ');
            haystack.push_str(label);
        }
    }
    contains_all(&haystack, query)
}

/// The settings of `section` the window shows for `query`, in table order.
pub fn rows_of(section: Section, query: &str) -> Vec<&'static SettingDef> {
    SETTINGS
        .iter()
        .filter(|def| def.section == section && is_shown(def) && matches(def, query))
        .collect()
}

/// The groups of shortcuts that answer `query`, each with only the shortcuts that do.
fn shortcuts_for(query: &str) -> Vec<(&'static str, Vec<&'static Shortcut>)> {
    KEYBOARD
        .iter()
        .filter_map(|group| {
            let found: Vec<&Shortcut> = group
                .shortcuts
                .iter()
                .filter(|s| contains_all(&format!("{} {} {}", group.name, s.keys, s.action), query))
                .collect();
            (!found.is_empty()).then_some((group.name, found))
        })
        .collect()
}

/// The actions of the Advanced section that answer `query`.
fn actions_for(query: &str) -> Vec<&'static ActionDef> {
    ACTIONS
        .iter()
        .filter(|a| contains_all(&format!("{} {}", a.label, a.help), query))
        .collect()
}

/// True when the About text answers `query`.
fn about_matches(query: &str) -> bool {
    contains_all(&about_lines().join(" "), query)
}

/// How many things of `section` answer `query`.
pub fn count_in(section: Section, query: &str) -> usize {
    let own = rows_of(section, query).len();
    own + match section {
        Section::Keyboard => shortcuts_for(query).iter().map(|(_, s)| s.len()).sum(),
        Section::Advanced => actions_for(query).len(),
        Section::About => usize::from(about_matches(query)),
        _ => 0,
    }
}

/// The sections the left column lists for `query`, with how many things of each match. With no
/// query, every section.
pub fn sections_with_matches(query: &str) -> Vec<(Section, usize)> {
    let searching = !query.trim().is_empty();
    SECTIONS
        .iter()
        .map(|section| (*section, count_in(*section, query)))
        .filter(|(_, count)| !searching || *count > 0)
        .collect()
}

/// How far one step of a stepper over `min..=max` goes: about 1/32 of the range, a power of two.
fn step_size(min: i64, max: i64) -> i64 {
    let span = u64::try_from(max.saturating_sub(min)).unwrap_or(0);
    i64::try_from((span / STEPS_ACROSS).max(1).next_power_of_two()).unwrap_or(1)
}

/// `current` moved one step up (`delta` > 0) or down, kept inside `min..=max`.
pub fn stepped_int(min: i64, max: i64, current: i64, delta: i64) -> i64 {
    let step = step_size(min, max);
    current
        .saturating_add(delta.signum().saturating_mul(step))
        .clamp(min, max)
}

/// The section and search text `DISK_ATLAS_OPEN_SETTINGS` asks for: `section[:search text]`.
pub fn parse_request(text: &str) -> (Option<Section>, String) {
    let (word, query) = text.split_once(':').unwrap_or((text, ""));
    (section_from_key(word), query.trim().to_owned())
}

/// `text` as one line of search text, cut to [`MAX_QUERY_BYTES`] on a char boundary.
fn clean_query(text: &str, room: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        if out.len() + ch.len_utf8() > room {
            break;
        }
        out.push(ch);
    }
    out
}

/// The folder saved search indexes live in, without its `index` leaf; `None` when the variables
/// it comes from are missing.
fn cache_folder() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if cfg!(windows) {
        Some(PathBuf::from(var("LOCALAPPDATA")?).join("DiskAtlas"))
    } else if cfg!(target_os = "macos") {
        Some(
            PathBuf::from(var("HOME")?)
                .join("Library")
                .join("Caches")
                .join("DiskAtlas"),
        )
    } else {
        let xdg = var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let base = match xdg {
            Some(xdg) => xdg,
            None => PathBuf::from(var("HOME")?).join(".cache"),
        };
        Some(base.join("disk-atlas"))
    }
}

/// Puts every setting of `section` that the window shows back to its default.
pub fn reset_section(cx: &mut App, section: Section) {
    for def in SETTINGS
        .iter()
        .filter(|def| def.section == section && is_shown(def))
    {
        Settings::reset(cx, def.key);
    }
}

/// Puts every setting the window shows back to its default.
pub fn reset_all(cx: &mut App) {
    for def in SETTINGS.iter().filter(|def| is_shown(def)) {
        Settings::reset(cx, def.key);
    }
}

// ---------------------------------------------------------------------------------------------
// The one window.

/// Row `index` of a menu was chosen. Dispatched to the control that opened the menu.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = settings_menu, no_json)]
struct Pick {
    index: usize,
}

/// The open Settings window, so a second request brings it forward.
#[derive(Default)]
struct SettingsSlot {
    handle: Option<AnyWindowHandle>,
    view: Option<WeakEntity<SettingsView>>,
}

impl Global for SettingsSlot {}

/// Opens the Settings window, or brings the open one to the front.
pub fn open(cx: &mut App) {
    show(cx, None, "");
}

/// [`open`], on `section` and with `query` in the search field when given.
pub fn show(cx: &mut App, section: Option<Section>, query: &str) {
    let live = cx
        .try_global::<SettingsSlot>()
        .and_then(|slot| slot.handle.zip(slot.view.clone()))
        .filter(|(handle, _)| cx.windows().contains(handle));
    if let Some((handle, view)) = live {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .ok();
        view.update(cx, |view, cx| view.apply_request(section, query, cx))
            .ok();
        return;
    }
    let options = AuxOptions {
        bounds_key: BOUNDS_KEY,
        title: "Settings".into(),
        size: size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)),
        min_size: size(px(MIN_SIZE.0), px(MIN_SIZE.1)),
        cascade: false,
    };
    let query = query.to_owned();
    match aux_window::open(options, cx, move |_, cx| {
        let view = cx.new(|cx| SettingsView::new(section, &query, cx));
        cx.default_global::<SettingsSlot>().view = Some(view.downgrade());
        view
    }) {
        Ok(handle) => cx.default_global::<SettingsSlot>().handle = Some(handle),
        Err(error) => tracing::error!(%error, "could not open the Settings window"),
    }
}

/// Debug builds: opens the window as `DISK_ATLAS_OPEN_SETTINGS` asks, for screenshots. Call it
/// once, after the main window is up.
pub fn open_from_env(cx: &mut App) {
    if !cfg!(debug_assertions) {
        return;
    }
    if let Ok(request) = std::env::var(OPEN_ENV) {
        let (section, query) = parse_request(&request);
        show(cx, section, &query);
    }
}

/// The body of the Settings window.
pub struct SettingsView {
    /// Holds the focus when the window opens; the root's key context lives here.
    focus: FocusHandle,
    search_focus: FocusHandle,
    sections_focus: FocusHandle,
    section_reset_focus: FocusHandle,
    action_focus: [FocusHandle; 3],
    query: String,
    section: Section,
    /// One tab stop per setting, made when its row is first drawn.
    controls: HashMap<&'static str, FocusHandle>,
    /// The Reset button of each setting.
    resets: HashMap<&'static str, FocusHandle>,
    /// Where each setting's menu button was last drawn, for placing its menu.
    anchors: HashMap<&'static str, Rc<Cell<Bounds<Pixels>>>>,
    menu: MenuSlot,
    /// Whose menu is open.
    menu_key: Option<&'static str>,
    /// "Reset every setting" has been asked once and waits for its answer.
    confirm_reset_all: bool,
    /// Rows added by tests, to draw kinds the table has none of.
    #[cfg(test)]
    extra: Vec<&'static SettingDef>,
    _settings: Subscription,
}

impl SettingsView {
    fn new(section: Option<Section>, query: &str, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            search_focus: cx.focus_handle().tab_stop(true),
            sections_focus: cx.focus_handle().tab_stop(true),
            section_reset_focus: cx.focus_handle().tab_stop(true),
            action_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            query: clean_query(query, MAX_QUERY_BYTES),
            section: section.unwrap_or(Section::General),
            controls: HashMap::new(),
            resets: HashMap::new(),
            anchors: HashMap::new(),
            menu: MenuSlot::default(),
            menu_key: None,
            confirm_reset_all: false,
            #[cfg(test)]
            extra: Vec::new(),
            _settings: cx.observe_global::<Settings>(|_, cx| cx.notify()),
        }
    }

    /// A second request for the window: moves to `section` and fills the field.
    fn apply_request(&mut self, section: Option<Section>, query: &str, cx: &mut Context<Self>) {
        if let Some(section) = section {
            self.section = section;
        }
        if !query.is_empty() || section.is_some() {
            self.query = clean_query(query, MAX_QUERY_BYTES);
        }
        cx.notify();
    }

    /// The section the right side shows: the chosen one, or the first with a match while it has
    /// none.
    fn current_section(&self) -> Section {
        let shown = sections_with_matches(&self.query);
        if shown.iter().any(|(section, _)| *section == self.section) {
            self.section
        } else {
            shown.first().map_or(self.section, |(section, _)| *section)
        }
    }

    fn control_focus(&mut self, key: &'static str, cx: &mut Context<Self>) -> FocusHandle {
        self.controls
            .entry(key)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    fn reset_focus(&mut self, key: &'static str, cx: &mut Context<Self>) -> FocusHandle {
        self.resets
            .entry(key)
            .or_insert_with(|| cx.focus_handle().tab_stop(true))
            .clone()
    }

    fn anchor(&mut self, key: &'static str) -> Rc<Cell<Bounds<Pixels>>> {
        Rc::clone(
            self.anchors
                .entry(key)
                .or_insert_with(|| Rc::new(Cell::new(Bounds::default()))),
        )
    }

    // Changes ----------------------------------------------------------------------------------

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query = query;
        cx.notify();
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    /// Up and Down in the list of sections.
    fn move_section(&mut self, delta: isize, cx: &mut Context<Self>) {
        let shown = sections_with_matches(&self.query);
        let current = self.current_section();
        let Some(at) = shown.iter().position(|(section, _)| *section == current) else {
            return;
        };
        let to = at
            .saturating_add_signed(delta)
            .min(shown.len().saturating_sub(1));
        self.select_section(shown[to].0, cx);
    }

    /// Left and Right on a segmented control or a stepper.
    fn nudge(&mut self, key: &'static str, delta: i64, cx: &mut Context<Self>) {
        let Some(def) = SETTINGS.iter().find(|def| def.key == key) else {
            return;
        };
        self.nudge_def(def, delta, cx);
    }

    fn nudge_def(&mut self, def: &'static SettingDef, delta: i64, cx: &mut Context<Self>) {
        match def.kind {
            SettingKind::Choice { options, .. } => {
                let current = Settings::get(cx).choice_of(def.key);
                let at = options.iter().position(|(value, _)| *value == current);
                let to = match at {
                    Some(at) => at
                        .saturating_add_signed(delta.signum() as isize)
                        .min(options.len().saturating_sub(1)),
                    None => 0,
                };
                if let Some((value, _)) = options.get(to) {
                    Settings::set(cx, def.key, SettingValue::Choice(value));
                }
            }
            SettingKind::Int { min, max, .. } => {
                let current = Settings::get(cx).int_of(def.key);
                Settings::set(
                    cx,
                    def.key,
                    SettingValue::Int(stepped_int(min, max, current, delta)),
                );
            }
            SettingKind::Bool { .. } | SettingKind::Hidden => {}
        }
    }

    fn set_bool(&mut self, key: &'static str, on: bool, cx: &mut Context<Self>) {
        Settings::set(cx, key, SettingValue::Bool(on));
    }

    fn set_choice(&mut self, key: &'static str, value: &'static str, cx: &mut Context<Self>) {
        Settings::set(cx, key, SettingValue::Choice(value));
    }

    /// The menu chose row `index` for the setting whose menu is open.
    fn pick(&mut self, key: &'static str, index: usize, cx: &mut Context<Self>) {
        let Some(def) = self.known_def(key) else {
            return;
        };
        if let SettingKind::Choice { options, .. } = def.kind
            && let Some((value, _)) = options.get(index)
        {
            Settings::set(cx, key, SettingValue::Choice(value));
        }
    }

    fn known_def(&self, key: &'static str) -> Option<&'static SettingDef> {
        #[cfg(test)]
        if let Some(def) = self.extra.iter().find(|def| def.key == key) {
            return Some(def);
        }
        SETTINGS.iter().find(|def| def.key == key)
    }

    fn open_menu(&mut self, def: &'static SettingDef, window: &mut Window, cx: &mut Context<Self>) {
        let SettingKind::Choice { options, .. } = def.kind else {
            return;
        };
        let current = Settings::get(cx).choice_of(def.key);
        let items: Vec<MenuItem> = options
            .iter()
            .enumerate()
            .map(|(index, (value, label))| {
                MenuItem::action(*label, Pick { index }).checked(*value == current)
            })
            .collect();
        let bounds = self.anchor(def.key).get();
        let position = point(bounds.left(), bounds.bottom() + space::S4);
        let focus = self.control_focus(def.key, cx);
        self.menu_key = Some(def.key);
        self.menu.show(
            items,
            position,
            Some(focus),
            window,
            cx,
            |this, _: CloseReason, _, cx| {
                this.menu.clear();
                this.menu_key = None;
                cx.notify();
            },
        );
    }

    fn run_action(&mut self, act: Act, cx: &mut Context<Self>) {
        match act {
            Act::OpenSettingsFile => {
                if let Some(path) = Settings::get(cx).file_path().map(PathBuf::from) {
                    cx.reveal_path(&path);
                }
            }
            Act::OpenCacheFolder => {
                if let Some(path) = cache_folder() {
                    cx.open_with_system(&path);
                }
            }
            Act::ResetAll => {
                self.confirm_reset_all = true;
                cx.notify();
            }
        }
    }

    fn escape(&mut self, _: &keys::Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_reset_all {
            self.confirm_reset_all = false;
            cx.notify();
        } else if !self.query.is_empty() {
            self.set_query(String::new(), cx);
        } else {
            aux_window::close_window(window, cx);
        }
    }

    fn focus_search(&mut self, _: &keys::FocusSearch, window: &mut Window, _: &mut Context<Self>) {
        window.focus(&self.search_focus);
    }

    /// A key typed in the search field.
    fn search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if keystroke.key == "backspace" {
            let mut query = self.query.clone();
            query.pop();
            self.set_query(query, cx);
        } else if keystroke.key == "down" || keystroke.key == "enter" {
            window.focus(&self.sections_focus);
        } else if modifiers.secondary() && keystroke.key == "v" {
            if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                let room = MAX_QUERY_BYTES.saturating_sub(self.query.len());
                let query = format!("{}{}", self.query, clean_query(&pasted, room));
                self.set_query(query, cx);
            }
        } else if !modifiers.control
            && !modifiers.platform
            && let Some(typed) = keystroke.key_char.as_deref()
            && !typed.chars().any(char::is_control)
        {
            let room = MAX_QUERY_BYTES.saturating_sub(self.query.len());
            let query = format!("{}{}", self.query, clean_query(typed, room));
            self.set_query(query, cx);
        }
    }

    // Drawing ----------------------------------------------------------------------------------

    /// The "Find a setting" field.
    fn render_search(
        &self,
        window: &Window,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let focused = self.search_focus.is_focused(window);
        let empty = self.query.is_empty();
        div()
            .id("settings-search")
            .track_focus(&self.search_focus)
            .on_key_down(cx.listener(Self::search_key))
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(sizes::CONTROL)
            .px(space::S8)
            .rounded(radius::FIELD)
            .bg(colors.field)
            .border_1()
            .border_color(if focused {
                colors.ink
            } else {
                transparent_black()
            })
            .text_size(text::BODY.size)
            .overflow_hidden()
            .child(if empty {
                div().text_color(colors.ink3).child("Find a setting")
            } else {
                div()
                    .text_color(colors.ink)
                    .whitespace_nowrap()
                    .child(self.query.clone())
            })
            .when(focused, |field| {
                field.child(
                    div()
                        .flex_none()
                        .w(metrics::CARET_WIDTH)
                        .h(text::BODY.size)
                        .bg(colors.ink),
                )
            })
    }

    /// The list of sections.
    fn render_sections(
        &self,
        shown: &[(Section, usize)],
        current: Section,
        window: &Window,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let searching = !self.query.trim().is_empty();
        let list_focused = self.sections_focus.is_focused(window);
        div()
            .id("settings-sections")
            .key_context(SETTINGS_SECTIONS)
            .track_focus(&self.sections_focus)
            .on_action(cx.listener(|this, _: &keys::SectionPrevious, _, cx| {
                this.move_section(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &keys::SectionNext, _, cx| {
                this.move_section(1, cx);
            }))
            .flex()
            .flex_col()
            .gap(space::S2)
            .children(shown.iter().enumerate().map(|(index, (section, count))| {
                let section = *section;
                let selected = section == current;
                div()
                    .id(("settings-section", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::S8)
                    .h(sizes::CONTROL)
                    .px(space::S8)
                    .rounded(radius::ROW)
                    .border_1()
                    .border_color(if selected && list_focused {
                        colors.ink
                    } else {
                        transparent_black()
                    })
                    .when(selected, |row| row.bg(colors.hover))
                    .when(!selected, |row| row.hover(|style| style.bg(colors.hover)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_section(section, cx);
                    }))
                    .text_size(text::BODY.size)
                    .text_color(if selected { colors.ink } else { colors.ink2 })
                    .child(
                        svg()
                            .path(section_glyph(section).path())
                            .flex_none()
                            .size(SECTION_GLYPH)
                            .text_color(colors.ink2),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(section_name(section)),
                    )
                    .when(searching, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(text::SMALL.size)
                                .text_color(colors.ink3)
                                .child(count.to_string()),
                        )
                    })
            }))
    }

    /// A plain text button.
    fn text_button(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        focus: &FocusHandle,
        colors: Colors,
        enabled: bool,
        filled: bool,
    ) -> Stateful<Div> {
        let (fill, ink) = if filled {
            (colors.ink, colors.sheet)
        } else {
            (colors.field, if enabled { colors.ink } else { colors.ink3 })
        };
        div()
            .id(id)
            .key_context(BUTTON)
            .track_focus(focus)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(sizes::CONTROL)
            .px(space::S12)
            .rounded(radius::FIELD)
            .border_1()
            .border_color(transparent_black())
            .bg(fill)
            .text_size(text::BODY.size)
            .text_color(ink)
            .whitespace_nowrap()
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.85))
                    .focus(move |style| style.border_color(colors.ink3))
            })
            .child(label.into())
    }

    /// Makes a button run `f` on a click or on Enter and Space.
    fn pressable(
        button: Stateful<Div>,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        button
            .on_action(
                cx.listener(move |this, _: &controls::Press, window, cx| f(this, window, cx)),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // Enter and Space arrive as `Press`; a keyboard "click" would run `f` twice.
                if !event.is_keyboard() {
                    f(this, window, cx);
                }
            }))
    }

    fn render_switch(
        &mut self,
        def: &'static SettingDef,
        row: usize,
        on: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = def.key;
        let focus = self.control_focus(key, cx);
        let thumb = div()
            .flex_none()
            .size(SWITCH_THUMB)
            .rounded_full()
            .bg(if on { colors.sheet } else { colors.ink3 });
        let track = div()
            .flex()
            .flex_row()
            .items_center()
            .w(SWITCH_WIDTH)
            .h(SWITCH_HEIGHT)
            .px(space::S2 + space::S2)
            .rounded_full()
            .border_1()
            .border_color(colors.line)
            .bg(if on { colors.ink } else { colors.field })
            .when(on, |track| track.justify_end())
            .child(thumb);
        let button = div()
            .id(("settings-control", row))
            .key_context(BUTTON)
            .track_focus(&focus)
            .flex()
            .items_center()
            .justify_center()
            .h(sizes::CONTROL)
            .px(space::S4)
            .rounded(radius::FIELD)
            .border_1()
            .border_color(transparent_black())
            .focus(move |style| style.border_color(colors.ink))
            .cursor_pointer()
            .child(track);
        Self::pressable(button, cx, move |this, _, cx| {
            let now = Settings::get(cx).bool_of(key);
            this.set_bool(key, !now, cx);
        })
        .into_any_element()
    }

    fn render_segments(
        &mut self,
        def: &'static SettingDef,
        row: usize,
        options: &'static [(&'static str, &'static str)],
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = def.key;
        let focus = self.control_focus(key, cx);
        let current = Settings::get(cx).choice_of(key);
        div()
            .id(("settings-control", row))
            .key_context(SETTINGS_SEGMENTS)
            .track_focus(&focus)
            .on_action(cx.listener(move |this, _: &keys::Previous, _, cx| this.nudge(key, -1, cx)))
            .on_action(cx.listener(move |this, _: &keys::Next, _, cx| this.nudge(key, 1, cx)))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(sizes::CONTROL)
            .px(space::S2)
            .rounded(radius::FIELD)
            .bg(colors.field)
            .border_1()
            .border_color(transparent_black())
            .focus(move |style| style.border_color(colors.ink))
            .children(options.iter().enumerate().map(|(index, (value, label))| {
                let value = *value;
                let selected = value == current;
                div()
                    .id(("settings-segment", row * 8 + index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(sizes::CONTROL_SEGMENT)
                    .px(space::S12)
                    .rounded(radius::ROW)
                    .text_size(text::BODY.size)
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .when(selected, |segment| {
                        segment.bg(colors.raised).text_color(colors.ink)
                    })
                    .when(!selected, |segment| {
                        segment
                            .text_color(colors.ink2)
                            .hover(|style| style.bg(colors.hover))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_choice(key, value, cx);
                    }))
                    .child(*label)
            }))
            .into_any_element()
    }

    fn render_menu_button(
        &mut self,
        def: &'static SettingDef,
        row: usize,
        options: &'static [(&'static str, &'static str)],
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = def.key;
        let focus = self.control_focus(key, cx);
        let anchor = self.anchor(key);
        let current = Settings::get(cx).choice_of(key);
        let label = options
            .iter()
            .find(|(value, _)| *value == current)
            .map_or("", |(_, label)| *label);
        let button = Self::text_button(
            ("settings-control", row),
            label,
            &focus,
            colors,
            true,
            false,
        )
        .gap(space::S8)
        .on_action(cx.listener(move |this, pick: &Pick, _, cx| this.pick(key, pick.index, cx)))
        .child(
            svg()
                .path(icon::CHEVRON)
                .flex_none()
                .w(metrics::CHEVRON_WIDTH)
                .h(metrics::CHEVRON_HEIGHT)
                .text_color(colors.ink2)
                .with_transformation(Transformation::rotate(radians(FRAC_PI_2))),
        )
        .child(
            canvas(move |bounds, _, _| anchor.set(bounds), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        )
        .relative();
        Self::pressable(button, cx, move |this, window, cx| {
            if let Some(def) = this.known_def(key) {
                this.open_menu(def, window, cx);
            }
        })
        .into_any_element()
    }

    fn render_stepper(
        &mut self,
        def: &'static SettingDef,
        row: usize,
        unit: &'static str,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = def.key;
        let focus = self.control_focus(key, cx);
        let value = Settings::get(cx).int_of(key);
        let step = |id: usize, glyph: &'static str, delta: i64, cx: &mut Context<Self>| {
            div()
                .id(("settings-step", row * 2 + id))
                .flex()
                .items_center()
                .justify_center()
                .size(sizes::CONTROL_SEGMENT)
                .rounded(radius::ROW)
                .text_size(text::BODY.size)
                .text_color(colors.ink)
                .cursor_pointer()
                .hover(|style| style.bg(colors.hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.nudge_def_by_key(key, delta, cx);
                }))
                .child(glyph)
        };
        div()
            .id(("settings-control", row))
            .key_context(SETTINGS_SEGMENTS)
            .track_focus(&focus)
            .on_action(cx.listener(move |this, _: &keys::Previous, _, cx| {
                this.nudge_def_by_key(key, -1, cx);
            }))
            .on_action(cx.listener(move |this, _: &keys::Next, _, cx| {
                this.nudge_def_by_key(key, 1, cx);
            }))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(sizes::CONTROL)
            .px(space::S2)
            .rounded(radius::FIELD)
            .bg(colors.field)
            .border_1()
            .border_color(transparent_black())
            .focus(move |style| style.border_color(colors.ink))
            .child(step(0, "\u{2212}", -1, cx))
            .child(
                div()
                    .px(space::S8)
                    .min_w(px(72.))
                    .flex()
                    .justify_center()
                    .text_size(text::BODY.size)
                    .text_color(colors.ink)
                    .whitespace_nowrap()
                    .child(format!("{value} {unit}")),
            )
            .child(step(1, "+", 1, cx))
            .into_any_element()
    }

    fn nudge_def_by_key(&mut self, key: &'static str, delta: i64, cx: &mut Context<Self>) {
        if let Some(def) = self.known_def(key) {
            self.nudge_def(def, delta, cx);
        }
    }

    /// One row of settings.
    fn render_row(
        &mut self,
        def: &'static SettingDef,
        row: usize,
        window: &Window,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let key = def.key;
        let is_default = Settings::get(cx).is_default(key);
        let control_focus = self.control_focus(key, cx);
        let reset_focus = self.reset_focus(key, cx);
        let revealed = control_focus.is_focused(window) || reset_focus.is_focused(window);
        let group: SharedString = format!("settings-row-{row}").into();

        let control = match def.kind {
            SettingKind::Bool { .. } => {
                let on = Settings::get(cx).bool_of(key);
                self.render_switch(def, row, on, colors, cx)
            }
            SettingKind::Choice { options, .. } if options.len() <= 4 => {
                self.render_segments(def, row, options, colors, cx)
            }
            SettingKind::Choice { options, .. } => {
                self.render_menu_button(def, row, options, colors, cx)
            }
            SettingKind::Int { unit, .. } => self.render_stepper(def, row, unit, colors, cx),
            SettingKind::Hidden => div().into_any_element(),
        };

        let reset = Self::text_button(
            ("settings-reset", row),
            "Reset",
            &reset_focus,
            colors,
            true,
            false,
        )
        .h(sizes::CONTROL_SEGMENT)
        .bg(transparent_black())
        .text_size(text::SMALL.size)
        .text_color(colors.ink2)
        .opacity(if revealed { 1. } else { 0. })
        .group_hover(group.clone(), |style| style.opacity(1.))
        .focus(move |style| style.opacity(1.).border_color(colors.ink));
        let reset = Self::pressable(reset, cx, move |_, _, cx| Settings::reset(cx, key));

        div()
            .id(("settings-row", row))
            .group(group)
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S12)
            .min_h(ROW_HEIGHT)
            .py(space::S8)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(space::S8)
                    .child(
                        // Always there, so a label does not move when it differs from its default.
                        div()
                            .flex_none()
                            .mt(space::S4 + space::S2)
                            .size(metrics::DOT)
                            .rounded_full()
                            .when(!is_default, |dot| dot.bg(colors.ink3)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(text::BODY.size)
                                    .text_color(colors.ink)
                                    .child(def.label),
                            )
                            .child(
                                div()
                                    .text_size(text::SMALL.size)
                                    .text_color(colors.ink2)
                                    .line_clamp(2)
                                    .child(def.help),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::S8)
                    .when(!is_default, |side| side.child(reset))
                    .child(control),
            )
    }

    /// "Reset this section".
    fn render_section_reset(
        &self,
        section: Section,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let any_changed = SETTINGS
            .iter()
            .filter(|def| def.section == section && is_shown(def))
            .any(|def| !Settings::get(cx).is_default(def.key));
        let button = Self::text_button(
            "settings-reset-section",
            "Reset this section",
            &self.section_reset_focus,
            colors,
            any_changed,
            false,
        );
        let button = if any_changed {
            Self::pressable(button, cx, move |_, _, cx| reset_section(cx, section))
        } else {
            button
        };
        div().mt(space::S24).flex().flex_row().child(button)
    }

    /// The head of a group inside a section.
    fn group_head(name: &'static str, first: bool, colors: Colors) -> Div {
        div()
            .when(!first, |head| head.mt(space::S24))
            .mb(space::S8)
            .text_size(text::SMALL_BOLD.size)
            .font_weight(text::SMALL_BOLD.weight)
            .text_color(colors.ink3)
            .child(name)
    }

    fn render_keyboard(&self, colors: Colors) -> Div {
        let mac = cfg!(target_os = "macos");
        let groups = shortcuts_for(&self.query);
        div()
            .flex()
            .flex_col()
            .children(
                groups
                    .into_iter()
                    .enumerate()
                    .map(|(at, (name, shortcuts))| {
                        div()
                            .flex()
                            .flex_col()
                            .child(Self::group_head(name, at == 0, colors))
                            .children(shortcuts.into_iter().map(|s| {
                                let keys = if mac {
                                    s.keys.replace("Ctrl", "Cmd")
                                } else {
                                    s.keys.to_owned()
                                };
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .gap(space::S12)
                                    .min_h(sizes::CONTROL)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(text::BODY.size)
                                            .text_color(colors.ink)
                                            .child(s.action),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .flex()
                                            .flex_row()
                                            .flex_wrap()
                                            .justify_end()
                                            .gap(space::S4)
                                            .children(keys.split(", ").map(|key| {
                                                div()
                                                    .px(space::S8)
                                                    .py(space::S2)
                                                    .rounded(radius::MARK)
                                                    .bg(colors.field)
                                                    .text_size(text::SMALL.size)
                                                    .text_color(colors.ink2)
                                                    .whitespace_nowrap()
                                                    .child(key.to_owned())
                                            })),
                                    )
                            }))
                    }),
            )
    }

    fn render_actions(&mut self, colors: Colors, cx: &mut Context<Self>) -> Div {
        let found = actions_for(&self.query);
        let have_file = Settings::get(cx).file_path().is_some();
        let have_cache = cache_folder().is_some();
        let mut list = div().flex().flex_col().mt(space::S24);
        list = list.child(Self::group_head("Actions", true, colors));
        for def in found {
            let act = def.act;
            let slot = ACTIONS.iter().position(|a| a.act == act).unwrap_or(0);
            let focus = self.action_focus[slot].clone();
            let enabled = match act {
                Act::OpenSettingsFile => have_file,
                Act::OpenCacheFolder => have_cache,
                Act::ResetAll => true,
            };
            let confirming = act == Act::ResetAll && self.confirm_reset_all;
            let side = if confirming {
                let cancel = Self::pressable(
                    Self::text_button(
                        "settings-reset-all-cancel",
                        "Cancel",
                        &self.action_focus[slot],
                        colors,
                        true,
                        false,
                    ),
                    cx,
                    |this, _, cx| {
                        this.confirm_reset_all = false;
                        cx.notify();
                    },
                );
                let really = Self::pressable(
                    Self::text_button(
                        "settings-reset-all-confirm",
                        "Reset every setting",
                        &self.section_reset_focus,
                        colors,
                        true,
                        true,
                    ),
                    cx,
                    |this, _, cx| {
                        this.confirm_reset_all = false;
                        reset_all(cx);
                        cx.notify();
                    },
                );
                div()
                    .flex()
                    .flex_row()
                    .gap(space::S8)
                    .child(cancel)
                    .child(really)
            } else {
                let button = Self::text_button(
                    ("settings-action", slot),
                    def.button,
                    &focus,
                    colors,
                    enabled,
                    false,
                );
                let button = if enabled {
                    Self::pressable(button, cx, move |this, _, cx| this.run_action(act, cx))
                } else {
                    button
                };
                div().flex().flex_row().child(button)
            };
            let help = if enabled {
                def.help
            } else {
                "Not available: the folder it lives in could not be found"
            };
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::S12)
                    .min_h(ROW_HEIGHT)
                    .py(space::S8)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(text::BODY.size)
                                    .text_color(colors.ink)
                                    .child(def.label),
                            )
                            .child(
                                div()
                                    .text_size(text::SMALL.size)
                                    .text_color(colors.ink2)
                                    .line_clamp(2)
                                    .child(help),
                            ),
                    )
                    .child(div().flex_none().child(side)),
            );
        }
        list
    }

    fn render_about(colors: Colors) -> Div {
        let lines = about_lines();
        div()
            .flex()
            .flex_col()
            .gap(space::S8)
            .children(lines.into_iter().enumerate().map(|(at, line)| {
                div()
                    .text_size(if at == 0 {
                        text::HEADING.size
                    } else {
                        text::BODY.size
                    })
                    .font_weight(if at == 0 {
                        text::HEADING.weight
                    } else {
                        text::BODY.weight
                    })
                    .text_color(if at == 0 { colors.ink } else { colors.ink2 })
                    .child(line)
            }))
    }

    /// The settings of `section` that answer the search.
    fn defs_in(&self, section: Section) -> Vec<&'static SettingDef> {
        #[allow(unused_mut, reason = "tests add rows")]
        let mut defs = rows_of(section, &self.query);
        #[cfg(test)]
        defs.extend(
            self.extra
                .iter()
                .copied()
                .filter(|def| def.section == section && matches(def, &self.query)),
        );
        defs
    }

    /// The right side: the section's name and its content.
    fn render_content(
        &mut self,
        section: Section,
        window: &Window,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let defs = self.defs_in(section);
        let rows: Vec<Stateful<Div>> = defs
            .iter()
            .enumerate()
            .map(|(row, def)| self.render_row(def, row, window, colors, cx))
            .collect();
        let has_table_rows = SETTINGS
            .iter()
            .any(|def| def.section == section && is_shown(def));
        let mut body = div().flex().flex_col().children(rows);
        match section {
            Section::Keyboard => body = body.child(self.render_keyboard(colors)),
            Section::Advanced => body = body.child(self.render_actions(colors, cx)),
            Section::About => body = body.child(Self::render_about(colors)),
            _ => {}
        }
        if has_table_rows {
            body = body.child(self.render_section_reset(section, colors, cx));
        }
        body.into_any_element()
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let shown = sections_with_matches(&self.query);
        let current = self.current_section();
        let nothing_found = shown.is_empty();

        let search = self.render_search(window, colors, cx);
        let sections = self.render_sections(&shown, current, window, colors, cx);
        let content = if nothing_found {
            div()
                .text_size(text::BODY.size)
                .text_color(colors.ink2)
                .child(format!(
                    "No setting matches \u{201c}{}\u{201d}. Try a shorter word, or press Escape to clear the search.",
                    self.query
                ))
                .into_any_element()
        } else {
            self.render_content(current, window, colors, cx)
        };

        div()
            .id("settings")
            .key_context(SETTINGS_WINDOW)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(|_, _: &FocusNext, window, _| window.focus_next()))
            .on_action(cx.listener(|_, _: &FocusPrev, window, _| window.focus_prev()))
            .size_full()
            .flex()
            .flex_row()
            .text_color(colors.ink)
            .child(
                div()
                    .flex_none()
                    .w(LIST_WIDTH)
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap(space::S12)
                    .p(space::S12)
                    .bg(colors.chrome)
                    .child(search)
                    .child(sections),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(colors.sheet)
                    .child(
                        div()
                            .flex_none()
                            .px(space::S24)
                            .pt(space::S24)
                            .pb(space::S12)
                            .text_size(text::TITLE.size)
                            .font_weight(text::TITLE.weight)
                            .text_color(colors.ink)
                            .child(section_name(current)),
                    )
                    .child(
                        div()
                            .id(("settings-scroll", current as usize))
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px(space::S24)
                            .pb(space::S24)
                            .child(content),
                    ),
            )
            // Out of the row's flow, so nothing moves while the menu is shown.
            .children(self.menu.entity().map(|menu| div().absolute().child(menu)))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{Entity, TestAppContext};

    use super::*;
    use crate::keymap;
    use crate::state::settings::Platforms;

    // Pure ---------------------------------------------------------------------------------

    #[test]
    fn a_word_of_the_help_line_finds_the_row() {
        let found = rows_of(Section::Browsing, "housekeeping");
        assert!(found.iter().any(|def| def.key == "show_system_items"));
        // Case does not matter, and every word must be there.
        assert!(!rows_of(Section::Browsing, "HOUSEKEEPING Thumbs").is_empty());
        assert!(rows_of(Section::Browsing, "housekeeping zebra").is_empty());
    }

    #[test]
    fn an_option_name_finds_the_row() {
        let found = rows_of(Section::QuickLook, "wireframe");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "ql_model_view");
    }

    #[test]
    fn a_query_matching_two_sections_lists_both_with_counts() {
        let shown = sections_with_matches("housekeeping");
        assert_eq!(
            shown,
            [(Section::Browsing, 1), (Section::Copying, 1)],
            "{shown:?}"
        );
        // No query: every section, none dropped.
        assert_eq!(sections_with_matches("").len(), SECTIONS.len());
        assert!(sections_with_matches("zzzzzz").is_empty());
    }

    #[test]
    fn the_sections_without_table_rows_answer_the_search_too() {
        assert_eq!(sections_with_matches("Alt+Up"), [(Section::Keyboard, 1)]);
        assert_eq!(
            sections_with_matches("cache folder"),
            [(Section::Advanced, 1)]
        );
        assert_eq!(sections_with_matches("never writes"), [(Section::About, 1)]);
    }

    #[test]
    fn hidden_rows_and_other_platforms_are_never_listed() {
        for section in SECTIONS {
            for def in rows_of(section, "") {
                assert!(!matches!(def.kind, SettingKind::Hidden), "{}", def.key);
                assert!(def.platforms.here(), "{}", def.key);
            }
        }
        let listed: usize = SECTIONS.iter().map(|s| rows_of(*s, "").len()).sum();
        let table = SETTINGS.iter().filter(|d| is_shown(d)).count();
        assert_eq!(listed, table, "every shown setting is in a listed section");
    }

    #[test]
    fn steppers_clamp_to_their_range() {
        assert_eq!(stepped_int(64, 4096, 256, 1), 384);
        assert_eq!(stepped_int(64, 4096, 256, -1), 128);
        assert_eq!(stepped_int(64, 4096, 4096, 1), 4096);
        assert_eq!(stepped_int(64, 4096, 64, -1), 64);
        assert_eq!(stepped_int(0, 10, 5, 1), 6);
        assert_eq!(stepped_int(5, 5, 5, 1), 5);
        assert_eq!(stepped_int(i64::MIN, i64::MAX, i64::MAX, 1), i64::MAX);
    }

    #[test]
    fn the_startup_request_names_a_section_and_a_search() {
        assert_eq!(
            parse_request("browsing"),
            (Some(Section::Browsing), String::new())
        );
        assert_eq!(
            parse_request("folder_sizes:watch"),
            (Some(Section::FolderSizes), "watch".to_owned())
        );
        assert_eq!(parse_request(":hidden"), (None, "hidden".to_owned()));
        assert_eq!(parse_request("nonsense"), (None, String::new()));
        for section in SECTIONS {
            assert_eq!(section_from_key(section_key(section)), Some(section));
        }
    }

    #[test]
    fn query_text_is_one_short_line() {
        assert_eq!(clean_query("a\nb\tc", 100), "abc");
        assert_eq!(clean_query("\u{e9}\u{e9}\u{e9}", 4), "\u{e9}\u{e9}");
    }

    // The window ---------------------------------------------------------------------------

    static SWITCH: SettingDef = SettingDef {
        key: "test_switch",
        section: Section::General,
        label: "A switch",
        help: "Switches",
        kind: SettingKind::Bool { default: false },
        platforms: Platforms::ALL,
    };
    static TWO: SettingDef = SettingDef {
        key: "test_two",
        section: Section::General,
        label: "Two",
        help: "Two options",
        kind: SettingKind::Choice {
            options: &[("a", "A"), ("b", "B")],
            default: "a",
        },
        platforms: Platforms::ALL,
    };
    static FOUR: SettingDef = SettingDef {
        key: "test_four",
        section: Section::General,
        label: "Four",
        help: "Four options",
        kind: SettingKind::Choice {
            options: &[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")],
            default: "a",
        },
        platforms: Platforms::ALL,
    };
    static SIX: SettingDef = SettingDef {
        key: "test_six",
        section: Section::General,
        label: "Six",
        help: "Six options, so a menu",
        kind: SettingKind::Choice {
            options: &[
                ("a", "A"),
                ("b", "B"),
                ("c", "C"),
                ("d", "D"),
                ("e", "E"),
                ("f", "F"),
            ],
            default: "a",
        },
        platforms: Platforms::ALL,
    };
    static NUMBER: SettingDef = SettingDef {
        key: "test_number",
        section: Section::General,
        label: "A number",
        help: "Counts",
        kind: SettingKind::Int {
            min: 1,
            max: 9,
            default: 3,
            unit: "things",
        },
        platforms: Platforms::ALL,
    };

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::theme::init(cx).ok();
            keymap::init(cx);
            cx.set_global(Settings::defaults());
            cx.on_window_closed(|cx| {
                aux_window::window_closed(cx);
            })
            .detach();
        });
    }

    /// The view alone in a window, with the synthetic rows added.
    fn view_window(
        cx: &mut TestAppContext,
        section: Section,
        extra: &[&'static SettingDef],
    ) -> (gpui::WindowHandle<SettingsView>, Entity<SettingsView>) {
        let extra = extra.to_vec();
        let window = cx.add_window(move |_, cx| {
            let mut view = SettingsView::new(Some(section), "", cx);
            view.extra = extra;
            view
        });
        let view = window.root(cx).expect("the window has its view");
        cx.run_until_parked();
        (window, view)
    }

    fn slot_handle(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| cx.global::<SettingsSlot>().handle)
            .expect("the window is open")
    }

    fn slot_view(cx: &mut TestAppContext) -> Entity<SettingsView> {
        cx.update(|cx| cx.global::<SettingsSlot>().view.clone())
            .and_then(|view| view.upgrade())
            .expect("the view is alive")
    }

    #[gpui::test]
    fn every_kind_of_setting_draws_a_row(cx: &mut TestAppContext) {
        setup(cx);
        let (_, view) = view_window(cx, Section::General, &[&SWITCH, &TWO, &FOUR, &SIX, &NUMBER]);
        view.read_with(cx, |view, _| {
            for key in [
                "test_switch",
                "test_two",
                "test_four",
                "test_six",
                "test_number",
            ] {
                assert!(view.controls.contains_key(key), "{key} was not drawn");
            }
            // The table's own rows of the section too.
            assert!(view.controls.contains_key("start_at"));
            assert!(view.controls.contains_key("confirm_recycle"));
        });
    }

    #[gpui::test]
    fn every_section_draws(cx: &mut TestAppContext) {
        setup(cx);
        let (_, view) = view_window(cx, Section::General, &[]);
        for section in SECTIONS {
            view.update(cx, |view, cx| view.select_section(section, cx));
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert_eq!(view.current_section(), section));
        }
        // A search with nothing found draws its message instead.
        view.update(cx, |view, cx| view.set_query("zzzzzz".into(), cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_change_made_in_the_window_shows_in_settings_and_wakes_observers(cx: &mut TestAppContext) {
        setup(cx);
        let woken = Rc::new(Cell::new(0));
        let counter = Rc::clone(&woken);
        cx.update(|cx| {
            cx.observe_global::<Settings>(move |_| counter.set(counter.get() + 1))
                .detach();
        });
        let (window, view) = view_window(cx, Section::Appearance, &[]);
        view.update(cx, |view, cx| {
            view.nudge("theme", 1, cx);
            view.nudge("density", -1, cx);
        });
        cx.update(|cx| {
            let settings = Settings::get(cx);
            assert_eq!(settings.choice_of("theme"), "day");
            assert_eq!(settings.choice_of("density"), "compact");
        });
        assert!(woken.get() >= 1, "no observer heard the changes");
        // Space on the focused switch of Browsing flips show_system_items.
        view.update(cx, |view, cx| view.select_section(Section::Browsing, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, window, _| {
                let focus = view.controls["show_system_items"].clone();
                window.focus(&focus);
            })
            .expect("window");
        cx.simulate_keystrokes(window.into(), "space");
        cx.update(|cx| assert!(Settings::get(cx).bool_of("show_system_items")));
        cx.simulate_keystrokes(window.into(), "space");
        cx.update(|cx| assert!(!Settings::get(cx).bool_of("show_system_items")));
    }

    #[gpui::test]
    fn right_and_left_move_a_segmented_control_and_stop_at_the_ends(cx: &mut TestAppContext) {
        setup(cx);
        let (window, _) = view_window(cx, Section::Appearance, &[]);
        window
            .update(cx, |view, window, _| {
                let focus = view.controls["density"].clone();
                window.focus(&focus);
            })
            .expect("window");
        cx.simulate_keystrokes(window.into(), "right right right");
        cx.update(|cx| assert_eq!(Settings::get(cx).choice_of("density"), "roomy"));
        cx.simulate_keystrokes(window.into(), "left");
        cx.update(|cx| assert_eq!(Settings::get(cx).choice_of("density"), "regular"));
    }

    #[gpui::test]
    fn the_stepper_moves_and_clamps(cx: &mut TestAppContext) {
        setup(cx);
        let (_, view) = view_window(cx, Section::Disks, &[]);
        view.update(cx, |view, cx| {
            for _ in 0..200 {
                view.nudge("cache_megabytes", 1, cx);
            }
        });
        cx.update(|cx| assert_eq!(Settings::get(cx).int_of("cache_megabytes"), 4096));
        view.update(cx, |view, cx| {
            for _ in 0..200 {
                view.nudge("cache_megabytes", -1, cx);
            }
        });
        cx.update(|cx| assert_eq!(Settings::get(cx).int_of("cache_megabytes"), 64));
    }

    #[gpui::test]
    fn a_menu_choice_sets_the_setting(cx: &mut TestAppContext) {
        setup(cx);
        let (_, view) = view_window(cx, Section::General, &[&SIX]);
        // A key that is not in the table is refused by Settings; that must not panic.
        view.update(cx, |view, cx| view.pick("test_six", 3, cx));
        view.update(cx, |view, cx| view.pick("size_units", 2, cx));
        cx.update(|cx| assert_eq!(Settings::get(cx).choice_of("size_units"), "binary"));
    }

    #[gpui::test]
    fn reset_restores_the_default_and_a_section_reset_resets_all_its_keys(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(|cx| {
            Settings::set(cx, "show_hidden", SettingValue::Bool(true));
            Settings::set(cx, "folders_first", SettingValue::Bool(false));
            Settings::set(cx, "theme", SettingValue::Choice("night"));
            assert!(!Settings::get(cx).is_default("show_hidden"));
            Settings::reset(cx, "show_hidden");
            assert!(Settings::get(cx).is_default("show_hidden"));
            assert!(!Settings::get(cx).bool_of("show_hidden"));
            Settings::set(cx, "show_hidden", SettingValue::Bool(true));
            reset_section(cx, Section::Browsing);
            let settings = Settings::get(cx);
            assert!(!settings.bool_of("show_hidden"));
            assert!(settings.bool_of("folders_first"));
            // Another section is left alone.
            assert_eq!(settings.choice_of("theme"), "night");
            reset_all(cx);
            assert_eq!(Settings::get(cx).choice_of("theme"), "system");
        });
    }

    #[gpui::test]
    fn escape_clears_the_search_then_closes_and_a_second_request_reuses_the_window(
        cx: &mut TestAppContext,
    ) {
        setup(cx);
        cx.update(|cx| show(cx, Some(Section::Browsing), "hidden"));
        cx.run_until_parked();
        let handle = slot_handle(cx);
        assert_eq!(cx.windows().len(), 1);
        // A second request brings the same window forward and applies its section.
        cx.update(|cx| show(cx, Some(Section::Keyboard), ""));
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        let view = slot_view(cx);
        view.read_with(cx, |view, _| assert_eq!(view.section, Section::Keyboard));
        cx.update(|cx| show(cx, Some(Section::Browsing), "hidden"));
        view.read_with(cx, |view, _| assert_eq!(view.query, "hidden"));
        cx.simulate_keystrokes(handle, "escape");
        view.read_with(cx, |view, _| assert_eq!(view.query, ""));
        assert!(cx.windows().contains(&handle));
        cx.simulate_keystrokes(handle, "escape");
        assert!(!cx.windows().contains(&handle));
        // Closed, it opens again.
        cx.update(open);
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
    }

    #[gpui::test]
    fn typing_filters_and_ctrl_f_focuses_the_field(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(open);
        cx.run_until_parked();
        let handle = slot_handle(cx);
        let view = slot_view(cx);
        cx.simulate_keystrokes(handle, "secondary-f");
        cx.simulate_input(handle, "housekeeping");
        view.read_with(cx, |view, _| {
            assert_eq!(view.query, "housekeeping");
            assert_eq!(view.current_section(), Section::Browsing);
        });
        cx.simulate_keystrokes(handle, "backspace");
        view.read_with(cx, |view, _| assert_eq!(view.query, "housekeepin"));
    }

    #[gpui::test]
    fn the_bounds_of_the_window_are_kept_in_settings(cx: &mut TestAppContext) {
        setup(cx);
        cx.update(open);
        cx.run_until_parked();
        let handle = slot_handle(cx);
        cx.simulate_keystrokes(handle, "escape");
        let saved = cx.update(|cx| aux_window::remembered(cx, BOUNDS_KEY));
        assert!(saved.is_some(), "closing remembers the bounds");
        cx.update(|cx| {
            assert!(Settings::get(cx).text_of("window.settings").is_some());
        });
    }
}
