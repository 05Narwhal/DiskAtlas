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

//! The settings store (`r10-everyday.md` 8.1). One table, [`SETTINGS`], describes every setting;
//! the file reader and writer and the typed [`Settings`] global are driven by it. The file is
//! `<config dir>/settings`: `key = value` lines, `#` comments, unknown keys and ignored bad lines
//! written back as they were. It is read once, before the first window opens, and written in the
//! background (temporary file in the same folder, then rename). Nothing here writes outside the
//! config folder.

// The settings window (R10b) and the views that read settings arrive later; until then most of
// this API has no caller.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use gpui::{App, Global};

/// The settings file's name inside the config folder.
const FILE_NAME: &str = "settings";
/// The temporary file a save writes first, in the same folder.
const TEMP_NAME: &str = "settings.tmp";
/// Keys that may repeat, one line per value.
const REPEATED_KEYS: [&str; 2] = ["pinned", "recent_image"];
/// Prefixes of the key families; each key of a family is one hidden text entry.
const FAMILY_PREFIXES: [&str; 2] = ["extension.", "hfs_encoding."];

/// A group of settings in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Appearance,
    Browsing,
    FolderSizes,
    Disks,
    QuickLook,
    Copying,
    Keyboard,
    Extensions,
    Advanced,
    About,
}

/// Where a setting exists.
#[derive(Debug, Clone, Copy)]
pub struct Platforms {
    pub windows: bool,
    pub macos: bool,
    pub linux: bool,
}

impl Platforms {
    /// Every platform.
    pub const ALL: Self = Self {
        windows: true,
        macos: true,
        linux: true,
    };

    /// Whether the platform this build runs on is included.
    pub const fn here(self) -> bool {
        if cfg!(windows) {
            self.windows
        } else if cfg!(target_os = "macos") {
            self.macos
        } else {
            self.linux
        }
    }
}

/// What a setting holds.
#[derive(Debug, Clone, Copy)]
pub enum SettingKind {
    Bool {
        default: bool,
    },
    /// `options` are (value, label); the value is the word written in the file.
    Choice {
        options: &'static [(&'static str, &'static str)],
        default: &'static str,
    },
    Int {
        min: i64,
        max: i64,
        default: i64,
        unit: &'static str,
    },
    /// Stored, not shown: window bounds, last destination, pins.
    Hidden,
}

/// One setting.
#[derive(Debug, Clone, Copy)]
pub struct SettingDef {
    pub key: &'static str,
    pub section: Section,
    pub label: &'static str,
    /// One line under the label. Says what changes, in plain words.
    pub help: &'static str,
    pub kind: SettingKind,
    /// Left out of the window (and ignored in the file) elsewhere.
    pub platforms: Platforms,
}

/// A value of a setting.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    /// The option's value word.
    Choice(&'static str),
    Int(i64),
    /// A hidden key's text.
    Text(String),
}

const fn boolean(
    key: &'static str,
    section: Section,
    label: &'static str,
    help: &'static str,
    default: bool,
) -> SettingDef {
    SettingDef {
        key,
        section,
        label,
        help,
        kind: SettingKind::Bool { default },
        platforms: Platforms::ALL,
    }
}

const fn choice(
    key: &'static str,
    section: Section,
    label: &'static str,
    help: &'static str,
    options: &'static [(&'static str, &'static str)],
    default: &'static str,
) -> SettingDef {
    SettingDef {
        key,
        section,
        label,
        help,
        kind: SettingKind::Choice { options, default },
        platforms: Platforms::ALL,
    }
}

const fn hidden(key: &'static str, section: Section) -> SettingDef {
    SettingDef {
        key,
        section,
        label: "",
        help: "",
        kind: SettingKind::Hidden,
        platforms: Platforms::ALL,
    }
}

/// Every setting, in the order the window shows them. Families (`extension.<name>`,
/// `hfs_encoding.<id>`) are not listed; see [`Settings::dynamic`].
pub static SETTINGS: &[SettingDef] = &[
    // General
    choice(
        "start_at",
        Section::General,
        "Open at",
        "Where Disk Atlas opens",
        &[
            ("last_place", "Last place"),
            ("this_computer", "This computer"),
        ],
        "last_place",
    ),
    choice(
        "size_units",
        Section::General,
        "Size units",
        "How sizes are counted",
        &[
            ("system", "As this system does"),
            ("decimal", "1 GB = 1000 MB"),
            ("binary", "1 GB = 1024 MB"),
        ],
        "system",
    ),
    boolean(
        "confirm_recycle",
        Section::General,
        "Confirm before recycling",
        "Ask before moving things to the Recycle Bin",
        true,
    ),
    // Appearance
    choice(
        "theme",
        Section::Appearance,
        "Theme",
        "Follow the system, or always use the day or night colours",
        &[("system", "System"), ("day", "Day"), ("night", "Night")],
        "system",
    ),
    choice(
        "density",
        Section::Appearance,
        "Row height",
        "How tall rows are in lists",
        &[
            ("compact", "Compact"),
            ("regular", "Regular"),
            ("roomy", "Roomy"),
        ],
        "regular",
    ),
    choice(
        "motion",
        Section::Appearance,
        "Motion",
        "Whether movements are animated or shown as a quick fade",
        &[
            ("system", "System"),
            ("reduced", "Reduced"),
            ("full", "Full"),
        ],
        "system",
    ),
    boolean(
        "window_material",
        Section::Appearance,
        "Translucent window",
        "Let the desktop show through the sidebar and top bar",
        true,
    ),
    choice(
        "storage_bar",
        Section::Appearance,
        "Storage bar",
        "The bar above the list that shows what fills the disk",
        &[
            ("full", "Full"),
            ("compact", "Compact"),
            ("hidden", "Hidden"),
        ],
        "full",
    ),
    boolean(
        "label_dots",
        Section::Appearance,
        "Label dots",
        "Show Finder labels as coloured dots",
        true,
    ),
    // Browsing
    choice(
        "view",
        Section::Browsing,
        "View",
        "Show folders as a list or as icons",
        &[("list", "List"), ("icons", "Icons")],
        "list",
    ),
    boolean(
        "folders_first",
        Section::Browsing,
        "Folders first",
        "Keep folders above files when sorting",
        true,
    ),
    boolean(
        "show_hidden",
        Section::Browsing,
        "Show hidden items",
        "Show files and folders the disk marks as hidden",
        false,
    ),
    boolean(
        "show_system_items",
        Section::Browsing,
        "Show housekeeping files",
        "Show housekeeping files such as .DS_Store, ._ files and Thumbs.db",
        false,
    ),
    boolean(
        "merge_sidecars",
        Section::Browsing,
        "Merge ._ files",
        "Treat a ._ file as part of the file it belongs to",
        true,
    ),
    boolean(
        "packages_as_folders",
        Section::Browsing,
        "Packages as folders",
        "Show applications and other packages as folders",
        false,
    ),
    boolean(
        "finder_views",
        Section::Browsing,
        "Use the Finder's views",
        "Open each folder in the view and order the Finder last used",
        false,
    ),
    choice(
        "finder_layout",
        Section::Browsing,
        "Use the Finder's icon places",
        "Lay icons out where the Finder had them",
        &[
            ("disk_images", "Disk images"),
            ("always", "Always"),
            ("never", "Never"),
        ],
        "disk_images",
    ),
    hidden("columns", Section::Browsing),
    boolean(
        "details_pane",
        Section::Browsing,
        "Details pane",
        "Show the details pane beside the list",
        true,
    ),
    boolean(
        "sidebar",
        Section::Browsing,
        "Sidebar",
        "Show the sidebar of places and disks",
        true,
    ),
    // Folder sizes
    choice(
        "folder_sizes",
        Section::FolderSizes,
        "Measure folders",
        "On this computer, which folders are measured",
        &[
            ("off", "Off"),
            ("open_folders", "Folders I open"),
            ("whole_drives", "Whole drives"),
        ],
        "open_folders",
    ),
    boolean(
        "folder_sizes_network",
        Section::FolderSizes,
        "Network and removable drives",
        "Also measure network and removable drives",
        false,
    ),
    boolean(
        "watch_changes",
        Section::FolderSizes,
        "Watch for changes",
        "Keep folders and their sizes up to date while files change",
        true,
    ),
    boolean(
        "index_on_open",
        Section::FolderSizes,
        "Read disks when opened",
        "Read a disk's whole file list when it is opened. Needed for folder sizes, the storage bar and instant search on disks Disk Atlas reads",
        true,
    ),
    // Disks
    boolean(
        "permission_notice",
        Section::Disks,
        "Permission notice",
        "Say at startup when disks need permission",
        true,
    ),
    boolean(
        "other_partitions_open",
        Section::Disks,
        "Open other partitions",
        "Show system and unreadable partitions without a click",
        false,
    ),
    boolean(
        "save_search_indexes",
        Section::Disks,
        "Save search indexes",
        "Keep a disk's search index so it opens fast next time",
        true,
    ),
    SettingDef {
        key: "cache_megabytes",
        section: Section::Disks,
        label: "Index memory",
        help: "How much memory search indexes may use",
        kind: SettingKind::Int {
            min: 64,
            max: 4096,
            default: 256,
            unit: "MB",
        },
        platforms: Platforms::ALL,
    },
    // Quick Look
    SettingDef {
        platforms: Platforms {
            windows: true,
            macos: true,
            linux: false,
        },
        ..boolean(
            "ql_keep_on_top",
            Section::QuickLook,
            "Keep Quick Look on top",
            "Keep the Quick Look window above other windows",
            false,
        )
    },
    boolean(
        "ql_remember_bounds",
        Section::QuickLook,
        "Remember its size and place",
        "Open Quick Look where and as large as you left it",
        true,
    ),
    choice(
        "ql_model_view",
        Section::QuickLook,
        "3D models",
        "How 3D models are drawn",
        &[("shaded", "Shaded"), ("wireframe", "Wireframe")],
        "shaded",
    ),
    // Copying
    boolean(
        "keep_forks_on_copy",
        Section::Copying,
        "Keep resource forks",
        "Keep Mac resource forks and Finder info when copying out of a disk",
        false,
    ),
    boolean(
        "copy_system_items",
        Section::Copying,
        "Copy housekeeping files",
        "Also copy housekeeping files found inside folders",
        false,
    ),
    choice(
        "conflicts",
        Section::Copying,
        "When a name is taken",
        "What happens when copying finds a file with the same name",
        &[("ask", "Ask"), ("keep_both", "Keep both"), ("skip", "Skip")],
        "ask",
    ),
    // Advanced
    choice(
        "log_level",
        Section::Advanced,
        "Log detail",
        "How much Disk Atlas writes to its log",
        &[
            ("errors", "Errors"),
            ("warnings", "Warnings"),
            ("everything", "Everything"),
        ],
        "warnings",
    ),
    // Hidden
    hidden("last_copy_destination", Section::Copying),
    hidden("pinned", Section::Browsing),
    hidden("recent_image", Section::Disks),
    hidden("window.main", Section::Advanced),
    hidden("window.quick_look", Section::Advanced),
    hidden("window.settings", Section::Advanced),
    hidden("window.properties", Section::Advanced),
    hidden("properties.collapsed", Section::Advanced),
];

/// The default text of a hidden key that has one.
fn hidden_default(key: &str) -> Option<&'static str> {
    match key {
        "columns" => Some("modified,size,kind"),
        _ => None,
    }
}

/// The definition of `key` on this platform. A key of another platform is unknown here.
fn def(key: &str) -> Option<&'static SettingDef> {
    SETTINGS
        .iter()
        .find(|def| def.key == key && def.platforms.here())
}

fn is_repeated(key: &str) -> bool {
    REPEATED_KEYS.contains(&key)
}

/// True for a key of a family, with a non-empty name after the prefix.
fn is_family_key(key: &str) -> bool {
    FAMILY_PREFIXES.iter().any(|prefix| {
        key.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Text that is safe to write as the value of one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Reads `text` as a value of `def`. The error says why it is refused.
fn parse_value(def: &SettingDef, text: &str) -> Result<SettingValue, String> {
    match def.kind {
        SettingKind::Bool { .. } => match text {
            "true" => Ok(SettingValue::Bool(true)),
            "false" => Ok(SettingValue::Bool(false)),
            _ => Err("not `true` or `false`".to_owned()),
        },
        SettingKind::Choice { options, .. } => options
            .iter()
            .find(|(value, _)| *value == text)
            .map(|(value, _)| SettingValue::Choice(value))
            .ok_or_else(|| "not one of its options".to_owned()),
        SettingKind::Int { min, max, .. } => {
            let number = text
                .parse::<i64>()
                .map_err(|_| "not a decimal number".to_owned())?;
            if (min..=max).contains(&number) {
                Ok(SettingValue::Int(number))
            } else {
                Err(format!("outside {min} to {max}"))
            }
        }
        SettingKind::Hidden => Ok(SettingValue::Text(one_line(text))),
    }
}

/// Checks a value given in code against `def`, giving the value to store.
fn validate(def: &SettingDef, value: SettingValue) -> Option<SettingValue> {
    match (def.kind, value) {
        (SettingKind::Bool { .. }, v @ SettingValue::Bool(_)) => Some(v),
        (SettingKind::Choice { options, .. }, SettingValue::Choice(word)) => options
            .iter()
            .find(|(value, _)| *value == word)
            .map(|(value, _)| SettingValue::Choice(value)),
        (SettingKind::Int { min, max, .. }, SettingValue::Int(n)) => {
            (min..=max).contains(&n).then_some(SettingValue::Int(n))
        }
        (SettingKind::Hidden, SettingValue::Text(text)) => {
            Some(SettingValue::Text(one_line(&text)))
        }
        _ => None,
    }
}

fn format_value(value: &SettingValue) -> String {
    match value {
        SettingValue::Bool(b) => b.to_string(),
        SettingValue::Choice(word) => (*word).to_owned(),
        SettingValue::Int(n) => n.to_string(),
        SettingValue::Text(text) => text.clone(),
    }
}

/// `(key, value)` of an entry line; `None` for blank lines, comments and lines without `=`.
fn split_entry(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// The operating systems whose config folder differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Other,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

/// The folder holding the settings file. `None` when the variables it comes from are missing:
/// then settings are not saved.
pub fn config_dir(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    match os {
        Os::Windows => Some(PathBuf::from(var("APPDATA")?).join("DiskAtlas")),
        Os::Mac => Some(
            PathBuf::from(var("HOME")?)
                .join("Library")
                .join("Application Support")
                .join("DiskAtlas"),
        ),
        Os::Other => {
            let xdg = var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute());
            let base = match xdg {
                Some(xdg) => xdg,
                None => PathBuf::from(var("HOME")?).join(".config"),
            };
            Some(base.join("disk-atlas"))
        }
    }
}

/// How many bytes a "kilo" is for the `size_units` choice `choice` on `os`.
pub fn size_unit_base(choice: &str, os: Os) -> u64 {
    match choice {
        "decimal" => 1000,
        "binary" => 1024,
        _ => match os {
            Os::Windows => 1024,
            Os::Mac | Os::Other => 1000,
        },
    }
}

/// Every setting's current value, plus the text of the file it came from.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Explicit, valid values. A key not here has its default.
    values: HashMap<&'static str, SettingValue>,
    /// Repeated keys, in file order.
    lists: HashMap<&'static str, Vec<String>>,
    /// Family keys (`extension.<name>`, `hfs_encoding.<id>`) and their text.
    dynamic: BTreeMap<String, String>,
    /// The file as read, line by line.
    lines: Vec<String>,
    /// Keys changed since the file was read. Only these are rewritten; every other line is
    /// written back as it was.
    dirty: BTreeSet<String>,
    /// Where to save. `None`: nowhere (no config folder, or the file could not be read).
    path: Option<PathBuf>,
}

impl Global for Settings {}

impl Settings {
    /// All defaults, no file.
    pub fn defaults() -> Self {
        Self {
            values: HashMap::new(),
            lists: HashMap::new(),
            dynamic: BTreeMap::new(),
            lines: Vec::new(),
            dirty: BTreeSet::new(),
            path: None,
        }
    }

    /// Reads the settings file of this machine. One small synchronous read, for startup before
    /// the first window. A missing file, or no config folder, gives the defaults.
    pub fn load() -> Self {
        Self::load_with(Os::current(), |name| std::env::var_os(name))
    }

    /// [`Settings::load`] with the operating system and environment given.
    pub fn load_with(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let Some(dir) = config_dir(os, env) else {
            tracing::warn!("no config folder found; settings will not be saved");
            return Self::defaults();
        };
        let path = dir.join(FILE_NAME);
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                // Do not save over a file that could not be read.
                tracing::warn!(%error, path = %path.display(), "settings file unreadable; using defaults and not saving");
                return Self::defaults();
            }
        };
        let (mut settings, warnings) = Self::parse(&text);
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        settings.path = Some(path);
        settings
    }

    /// Reads settings text. Returns the settings (which keep every line for writing back) and
    /// one warning per line that was ignored.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut settings = Self::defaults();
        let mut warnings = Vec::new();
        for line in text.lines() {
            settings.lines.push(line.to_owned());
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match split_entry(line) {
                Some((key, value)) => settings.load_entry(key, value, &mut warnings),
                None => warnings.push(format!("settings: ignoring `{trimmed}`: no `=`")),
            }
        }
        (settings, warnings)
    }

    fn load_entry(&mut self, key: &str, value: &str, warnings: &mut Vec<String>) {
        if let Some(def) = def(key) {
            if is_repeated(def.key) {
                if !value.is_empty() {
                    self.lists.entry(def.key).or_default().push(one_line(value));
                }
                return;
            }
            match parse_value(def, value) {
                Ok(parsed) => {
                    self.values.insert(def.key, parsed);
                }
                Err(why) => warnings.push(format!(
                    "settings: ignoring `{key} = {value}`: {why}; using the default"
                )),
            }
        } else if is_family_key(key) {
            self.dynamic.insert(key.to_owned(), value.to_owned());
        }
        // Anything else is unknown here and stays in `lines`.
    }

    /// The text to write: the lines as read, with every changed key rewritten (at its first
    /// line, or at the end) and the rest untouched.
    pub fn render(&self) -> String {
        let mut out: Vec<String> = Vec::with_capacity(self.lines.len() + self.dirty.len());
        let mut done: BTreeSet<&str> = BTreeSet::new();
        for line in &self.lines {
            match split_entry(line) {
                Some((key, _)) if self.dirty.contains(key) => {
                    if done.insert(key) {
                        self.entry_lines(key, &mut out);
                    }
                }
                _ => out.push(line.clone()),
            }
        }
        for key in &self.dirty {
            if !done.contains(key.as_str()) {
                self.entry_lines(key, &mut out);
            }
        }
        let mut text = out.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        text
    }

    fn entry_lines(&self, key: &str, out: &mut Vec<String>) {
        if let Some(def) = def(key) {
            if is_repeated(def.key) {
                if let Some(list) = self.lists.get(def.key) {
                    out.extend(list.iter().map(|value| format!("{key} = {value}")));
                }
            } else if let Some(value) = self.values.get(def.key) {
                out.push(format!("{key} = {}", format_value(value)));
            }
        } else if let Some(text) = self.dynamic.get(key) {
            out.push(format!("{key} = {text}"));
        }
    }

    // Typed reads. A key of the wrong kind, or not in the table, gives the neutral value.

    pub fn bool_of(&self, key: &str) -> bool {
        match self.values.get(key) {
            Some(SettingValue::Bool(b)) => *b,
            _ => matches!(
                def(key).map(|d| d.kind),
                Some(SettingKind::Bool { default: true })
            ),
        }
    }

    pub fn choice_of(&self, key: &str) -> &'static str {
        match self.values.get(key) {
            Some(SettingValue::Choice(word)) => word,
            _ => match def(key).map(|d| d.kind) {
                Some(SettingKind::Choice { default, .. }) => default,
                _ => "",
            },
        }
    }

    pub fn int_of(&self, key: &str) -> i64 {
        match self.values.get(key) {
            Some(SettingValue::Int(n)) => *n,
            _ => match def(key).map(|d| d.kind) {
                Some(SettingKind::Int { default, .. }) => default,
                _ => 0,
            },
        }
    }

    /// The text of a hidden single-valued key, or its default if it has one.
    pub fn text_of(&self, key: &str) -> Option<&str> {
        match self.values.get(key) {
            Some(SettingValue::Text(text)) => Some(text),
            _ => def(key)
                .filter(|d| matches!(d.kind, SettingKind::Hidden))
                .and_then(|d| hidden_default(d.key)),
        }
    }

    /// The values of a repeated key (`pinned`, `recent_image`), in file order.
    pub fn list_of(&self, key: &str) -> &[String] {
        self.lists.get(key).map_or(&[], Vec::as_slice)
    }

    /// The text of a family key such as `extension.<name>`.
    pub fn dynamic(&self, key: &str) -> Option<&str> {
        self.dynamic.get(key).map(String::as_str)
    }

    /// How many bytes a "kilo" is, from the `size_units` setting.
    pub fn size_unit_base(&self) -> u64 {
        size_unit_base(self.choice_of("size_units"), Os::current())
    }

    /// True when `key` holds its default. A key not in the table counts as default.
    pub fn is_default(&self, key: &str) -> bool {
        match def(key).map(|d| d.kind) {
            Some(SettingKind::Bool { default }) => self.bool_of(key) == default,
            Some(SettingKind::Choice { default, .. }) => self.choice_of(key) == default,
            Some(SettingKind::Int { default, .. }) => self.int_of(key) == default,
            Some(SettingKind::Hidden) => {
                self.text_of(key) == hidden_default(key) && self.list_of(key).is_empty()
            }
            None => true,
        }
    }

    /// The file this store saves to, if there is one.
    pub fn file_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    // Changes. Each returns whether anything changed.

    fn apply(&mut self, key: &'static str, value: SettingValue) -> bool {
        let Some(def) = def(key) else {
            tracing::warn!(key, "settings: unknown key");
            return false;
        };
        if is_repeated(key) {
            tracing::warn!(key, "settings: a repeated key is set with set_list");
            return false;
        }
        let Some(value) = validate(def, value) else {
            tracing::warn!(key, "settings: value refused");
            return false;
        };
        let value = match value {
            SettingValue::Text(text) if text.is_empty() => None,
            other => Some(other),
        };
        if self.values.get(def.key) == value.as_ref() {
            return false;
        }
        match value {
            Some(value) => {
                self.values.insert(def.key, value);
            }
            None => {
                self.values.remove(def.key);
            }
        }
        self.dirty.insert(def.key.to_owned());
        true
    }

    fn apply_reset(&mut self, key: &'static str) -> bool {
        let Some(def) = def(key) else {
            return false;
        };
        let had = self.values.remove(def.key).is_some() | self.lists.remove(def.key).is_some();
        // A bad line for this key is dropped too: the user asked for the default.
        let in_file = self
            .lines
            .iter()
            .any(|line| split_entry(line).is_some_and(|(k, _)| k == key));
        if had || in_file {
            self.dirty.insert(def.key.to_owned());
        }
        had || in_file
    }

    fn apply_list(&mut self, key: &'static str, values: Vec<String>) -> bool {
        if !is_repeated(key) || def(key).is_none() {
            tracing::warn!(key, "settings: not a repeated key");
            return false;
        }
        let values: Vec<String> = values
            .iter()
            .map(|v| one_line(v))
            .filter(|v| !v.is_empty())
            .collect();
        if self.list_of(key) == values.as_slice() {
            return false;
        }
        if values.is_empty() {
            self.lists.remove(key);
        } else {
            self.lists.insert(key, values);
        }
        self.dirty.insert(key.to_owned());
        true
    }

    fn apply_dynamic(&mut self, key: &str, value: Option<String>) -> bool {
        if !is_family_key(key) || key.contains(['=', '\n', '\r']) || key != key.trim() {
            tracing::warn!(key, "settings: not a family key");
            return false;
        }
        let changed = match value.map(|v| one_line(&v)) {
            Some(value) => self.dynamic.insert(key.to_owned(), value.clone()) != Some(value),
            None => self.dynamic.remove(key).is_some(),
        };
        if changed {
            self.dirty.insert(key.to_owned());
        }
        changed
    }

    /// The write a change calls for, if there is somewhere to write.
    fn save_job(&self) -> Option<SaveJob> {
        self.path.clone().map(|path| (path, self.render()))
    }

    // The global.

    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    /// Sets one key, repaints through `observe_global`, saves in the background. A value the
    /// key does not accept is refused with a warning and changes nothing.
    pub fn set(cx: &mut App, key: &'static str, value: SettingValue) {
        Self::update(cx, |settings| settings.apply(key, value));
    }

    /// Blocks until a save in flight is on disk, for at most [`FLUSH_TIMEOUT`]. Called once as the
    /// app quits: the worker is detached, and nothing else waits for it.
    pub fn flush() {
        SAVE_QUEUE.flush(FLUSH_TIMEOUT);
    }

    /// Puts one key back to its default and removes its line from the file.
    pub fn reset(cx: &mut App, key: &'static str) {
        Self::update(cx, |settings| settings.apply_reset(key));
    }

    /// Replaces the values of a repeated key (`pinned`, `recent_image`).
    pub fn set_list(cx: &mut App, key: &'static str, values: Vec<String>) {
        Self::update(cx, |settings| settings.apply_list(key, values));
    }

    /// Sets (or with `None` removes) a family key such as `extension.<name>`.
    pub fn set_dynamic(cx: &mut App, key: &str, value: Option<String>) {
        Self::update(cx, |settings| settings.apply_dynamic(key, value));
    }

    fn update(cx: &mut App, change: impl FnOnce(&mut Settings) -> bool) {
        let mut next = cx.global::<Settings>().clone();
        if !change(&mut next) {
            return;
        }
        let job = next.save_job();
        cx.set_global(next);
        if let Some(job) = job
            && SAVE_QUEUE.submit(job)
        {
            cx.background_executor()
                .spawn(async { SAVE_QUEUE.run() })
                .detach();
        }
    }
}

type SaveJob = (PathBuf, String);

/// The longest quitting waits for the settings to be written.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The newest write waiting, and whether a worker is draining the queue. A burst of changes
/// leaves one job and one worker.
struct SaveQueue {
    state: Mutex<QueueState>,
    /// Notified when the worker ends, so [`SaveQueue::flush`] can stop waiting.
    idle: Condvar,
}

struct QueueState {
    pending: Option<SaveJob>,
    running: bool,
}

static SAVE_QUEUE: SaveQueue = SaveQueue::new();

impl SaveQueue {
    const fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                pending: None,
                running: false,
            }),
            idle: Condvar::new(),
        }
    }

    /// Queues `job`, replacing any older one. True when the caller must start a worker.
    fn submit(&self, job: SaveJob) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending = Some(job);
        !std::mem::replace(&mut state.running, true)
    }

    /// The next job; `None` (and the worker's end) when nothing waits.
    fn next(&self) -> Option<SaveJob> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let job = state.pending.take();
        if job.is_none() {
            state.running = false;
            self.idle.notify_all();
        }
        job
    }

    /// Waits until the worker has written everything queued, for at most `timeout`. When no
    /// worker is running but a job is waiting (the worker never got to start), writes it here:
    /// this thread then is the one writer, so the temporary file is never used by two.
    fn flush(&self, timeout: Duration) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = self
            .idle
            .wait_timeout_while(state, timeout, |state| state.running)
            .unwrap_or_else(PoisonError::into_inner);
        if !state.running && state.pending.is_some() {
            state.running = true;
            drop(state);
            self.run();
        }
    }

    /// The worker: writes jobs until none is left.
    fn run(&self) {
        while let Some((path, text)) = self.next() {
            if let Err(error) = write_atomic(&path, &text) {
                tracing::error!(%error, path = %path.display(), "could not save settings");
            }
        }
    }
}

/// Writes `text` to `path` through a temporary file in the same folder, then renames it over.
/// Only the settings file and its temporary sibling are touched.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP_NAME);
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs = pairs.to_vec();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn temp_folder() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "disk-atlas-settings-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp folder");
        dir
    }

    #[test]
    fn table_is_consistent() {
        let mut seen = BTreeSet::new();
        for def in SETTINGS {
            assert!(seen.insert(def.key), "duplicate key {}", def.key);
            match def.kind {
                SettingKind::Hidden => {}
                SettingKind::Bool { .. } => assert!(!def.help.is_empty(), "{}", def.key),
                SettingKind::Choice { options, default } => {
                    assert!(!def.help.is_empty(), "{}", def.key);
                    assert!(
                        options.iter().any(|(value, _)| *value == default),
                        "{} default is not an option",
                        def.key
                    );
                    assert!(options.iter().all(|(v, l)| !v.is_empty() && !l.is_empty()));
                }
                SettingKind::Int {
                    min, max, default, ..
                } => {
                    assert!(!def.help.is_empty(), "{}", def.key);
                    assert!(min <= default && default <= max, "{}", def.key);
                }
            }
            if !matches!(def.kind, SettingKind::Hidden) {
                assert!(!def.label.is_empty(), "{}", def.key);
            }
        }
        for key in REPEATED_KEYS {
            assert!(seen.contains(key), "{key} is not in the table");
        }
    }

    #[test]
    fn defaults_match_the_design() {
        let s = Settings::defaults();
        assert_eq!(s.choice_of("folder_sizes"), "open_folders");
        assert_eq!(s.choice_of("size_units"), "system");
        assert!(s.bool_of("confirm_recycle"));
        assert!(!s.bool_of("show_hidden"));
        assert_eq!(s.int_of("cache_megabytes"), 256);
        assert_eq!(s.text_of("columns"), Some("modified,size,kind"));
        assert_eq!(s.text_of("window.main"), None);
    }

    #[test]
    fn size_units_resolve_per_os() {
        assert_eq!(size_unit_base("system", Os::Windows), 1024);
        assert_eq!(size_unit_base("system", Os::Mac), 1000);
        assert_eq!(size_unit_base("system", Os::Other), 1000);
        assert_eq!(size_unit_base("decimal", Os::Windows), 1000);
        assert_eq!(size_unit_base("binary", Os::Mac), 1024);
    }

    #[test]
    fn round_trip() {
        let mut s = Settings::defaults();
        assert!(s.apply("folder_sizes", SettingValue::Choice("whole_drives")));
        assert!(s.apply("show_hidden", SettingValue::Bool(true)));
        assert!(s.apply("cache_megabytes", SettingValue::Int(512)));
        assert!(s.apply("window.main", SettingValue::Text("10,20,1180,760".into())));
        assert!(s.apply_list("pinned", vec!["a:1:Docs".into(), "b:2:Pics".into()]));
        assert!(s.apply_dynamic("extension.demo", Some("true".into())));
        let text = s.render();
        let (back, warnings) = Settings::parse(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back.choice_of("folder_sizes"), "whole_drives");
        assert!(back.bool_of("show_hidden"));
        assert_eq!(back.int_of("cache_megabytes"), 512);
        assert_eq!(back.text_of("window.main"), Some("10,20,1180,760"));
        assert_eq!(back.list_of("pinned"), ["a:1:Docs", "b:2:Pics"]);
        assert_eq!(back.dynamic("extension.demo"), Some("true"));
        assert_eq!(back.render(), text);
    }

    #[test]
    fn unknown_keys_and_comments_survive() {
        let text = "# my settings\n\nfuture_key = 7\ntheme = night\n# end\n";
        let (mut s, warnings) = Settings::parse(text);
        assert!(warnings.is_empty());
        assert_eq!(s.render(), text);
        assert!(s.apply("theme", SettingValue::Choice("day")));
        assert_eq!(
            s.render(),
            "# my settings\n\nfuture_key = 7\ntheme = day\n# end\n"
        );
        assert!(s.apply("view", SettingValue::Choice("icons")));
        assert!(s.render().starts_with("# my settings\n"));
        assert!(s.render().ends_with("# end\nview = icons\n"));
    }

    #[test]
    fn bad_value_is_ignored_logged_once_and_kept() {
        let text = "folder_sizes = everything\nshow_hidden = maybe\n";
        let (mut s, warnings) = Settings::parse(text);
        assert_eq!(warnings.len(), 2);
        assert_eq!(s.choice_of("folder_sizes"), "open_folders");
        assert!(!s.bool_of("show_hidden"));
        assert_eq!(s.render(), text);
        assert!(s.apply("folder_sizes", SettingValue::Choice("off")));
        assert_eq!(s.render(), "folder_sizes = off\nshow_hidden = maybe\n");
    }

    #[test]
    fn out_of_range_int_is_ignored() {
        let (mut s, warnings) = Settings::parse("cache_megabytes = 99999\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(s.int_of("cache_megabytes"), 256);
        assert_eq!(s.render(), "cache_megabytes = 99999\n");
        assert!(!s.apply("cache_megabytes", SettingValue::Int(10)));
        assert!(!s.apply("cache_megabytes", SettingValue::Int(-5)));
        assert_eq!(s.int_of("cache_megabytes"), 256);
        let (s, warnings) = Settings::parse("cache_megabytes = 12abc\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(s.int_of("cache_megabytes"), 256);
    }

    #[test]
    fn wrong_kind_and_unknown_choice_are_refused() {
        let mut s = Settings::defaults();
        assert!(!s.apply("show_hidden", SettingValue::Int(1)));
        assert!(!s.apply("theme", SettingValue::Choice("purple")));
        assert!(!s.apply("no_such_key", SettingValue::Bool(true)));
        assert!(!s.apply("pinned", SettingValue::Text("x".into())));
        assert_eq!(s.render(), "");
    }

    #[test]
    fn reset_removes_the_line() {
        let (mut s, _) = Settings::parse("# c\ntheme = night\nview = icons\n");
        assert!(s.apply_reset("theme"));
        assert_eq!(s.choice_of("theme"), "system");
        assert_eq!(s.render(), "# c\nview = icons\n");
    }

    #[test]
    fn text_values_cannot_inject_lines() {
        let mut s = Settings::defaults();
        assert!(s.apply(
            "last_copy_destination",
            SettingValue::Text("a\ntheme = night".into())
        ));
        assert_eq!(s.render().lines().count(), 1);
        assert!(!s.apply_dynamic("extension.x=y", Some("1".into())));
    }

    #[test]
    fn repeated_keys_keep_order() {
        let text = "pinned = c\ntheme = day\npinned = a\nrecent_image = z\npinned = b\n";
        let (mut s, warnings) = Settings::parse(text);
        assert!(warnings.is_empty());
        assert_eq!(s.list_of("pinned"), ["c", "a", "b"]);
        assert_eq!(s.render(), text);
        assert!(s.apply_list("pinned", vec!["b".into(), "c".into()]));
        assert_eq!(
            s.render(),
            "pinned = b\npinned = c\ntheme = day\nrecent_image = z\n"
        );
    }

    #[test]
    fn config_dir_per_os() {
        let win = env_of(&[("APPDATA", "C:\\Users\\j\\AppData\\Roaming")]);
        assert_eq!(
            config_dir(Os::Windows, win),
            Some(PathBuf::from("C:\\Users\\j\\AppData\\Roaming").join("DiskAtlas"))
        );
        assert_eq!(config_dir(Os::Windows, env_of(&[])), None);
        assert_eq!(
            config_dir(Os::Mac, env_of(&[("HOME", "/Users/j")])),
            Some(PathBuf::from(
                "/Users/j/Library/Application Support/DiskAtlas"
            ))
        );
        assert_eq!(
            config_dir(Os::Other, env_of(&[("HOME", "/home/j")])),
            Some(PathBuf::from("/home/j/.config/disk-atlas"))
        );
        let xdg = if cfg!(windows) { "C:\\cfg" } else { "/cfg" };
        let xdg_env = |name: &str| match name {
            "HOME" => Some(OsString::from("/home/j")),
            "XDG_CONFIG_HOME" => Some(OsString::from(xdg)),
            _ => None,
        };
        assert_eq!(
            config_dir(Os::Other, xdg_env),
            Some(PathBuf::from(xdg).join("disk-atlas"))
        );
        // A relative XDG_CONFIG_HOME is ignored, as the spec says.
        assert_eq!(
            config_dir(
                Os::Other,
                env_of(&[("HOME", "/home/j"), ("XDG_CONFIG_HOME", "rel")])
            ),
            Some(PathBuf::from("/home/j/.config/disk-atlas"))
        );
        assert_eq!(config_dir(Os::Other, env_of(&[])), None);
    }

    #[test]
    fn write_is_temp_then_rename() {
        let root = temp_folder();
        let dir = root.join("nested");
        let path = dir.join(FILE_NAME);
        write_atomic(&path, "theme = day\n").expect("first write");
        write_atomic(&path, "theme = night\n").expect("overwrite");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme = night\n");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from(FILE_NAME)], "no temp file is left");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_reads_the_file_and_a_missing_one_gives_defaults() {
        let root = temp_folder();
        let cfg = root.join("cfg");
        let cfg_text = cfg.to_string_lossy().into_owned();
        let env = |name: &str| match name {
            "XDG_CONFIG_HOME" => Some(OsString::from(&cfg_text)),
            _ => None,
        };
        let missing = Settings::load_with(Os::Other, env);
        assert_eq!(missing.choice_of("theme"), "system");
        assert!(missing.path.is_some());
        std::fs::create_dir_all(cfg.join("disk-atlas")).unwrap();
        std::fs::write(cfg.join("disk-atlas").join(FILE_NAME), "theme = night\n").unwrap();
        let loaded = Settings::load_with(Os::Other, env);
        assert_eq!(loaded.choice_of("theme"), "night");
        let none = Settings::load_with(Os::Other, env_of(&[]));
        assert!(none.path.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn flush_writes_a_job_nobody_started_a_worker_for() {
        let root = temp_folder();
        let path = root.join(FILE_NAME);
        let queue = SaveQueue::new();
        // The caller was told to start a worker, and the worker never ran.
        assert!(
            queue.submit((
                path.clone(),
                "theme = day
"
                .into()
            ))
        );
        assert!(
            !queue.submit((
                path.clone(),
                "theme = night
"
                .into()
            ))
        );
        // A worker is "running" until it ends, so a flush now only waits; end it as the worker
        // would, with the job still queued.
        queue.state.lock().unwrap().running = false;
        queue.flush(Duration::from_secs(5));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme = night
"
        );
        assert!(queue.submit((path, "x".into())), "the queue is idle again");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn flush_waits_for_the_worker_and_the_newest_text_is_on_disk() {
        let root = temp_folder();
        let path = root.join(FILE_NAME);
        let queue = SaveQueue::new();
        assert!(
            queue.submit((
                path.clone(),
                "theme = day
"
                .into()
            ))
        );
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // The worker starts late and finds a newer job than the one it was told about.
                std::thread::sleep(Duration::from_millis(50));
                queue.run();
            });
            assert!(
                !queue.submit((
                    path.clone(),
                    "theme = night
"
                    .into()
                ))
            );
            queue.flush(Duration::from_secs(5));
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "theme = night
"
            );
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn flush_gives_up_when_the_worker_is_stuck() {
        let queue = SaveQueue::new();
        assert!(queue.submit((PathBuf::from("a"), "1".into())));
        // No worker ever ends: quitting is not held up past the timeout.
        let started = std::time::Instant::now();
        queue.flush(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(2));
        // The job is still waiting; flush did not write it behind the worker's back.
        assert!(queue.state.lock().unwrap().pending.is_some());
    }

    #[test]
    fn saves_coalesce_into_one_worker_and_the_newest_job() {
        let queue = SaveQueue::new();
        assert!(queue.submit((PathBuf::from("a"), "1".into())));
        assert!(!queue.submit((PathBuf::from("a"), "2".into())));
        assert!(!queue.submit((PathBuf::from("a"), "3".into())));
        assert_eq!(queue.next().map(|(_, text)| text), Some("3".to_owned()));
        assert!(queue.next().is_none());
        // The worker is over; the next change starts another.
        assert!(queue.submit((PathBuf::from("a"), "4".into())));
    }
}
