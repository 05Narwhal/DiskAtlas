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

//! The file list: a column header and a virtualized list of the current folder's rows.
//!
//! Rows are built only for the visible range, straight from `Listing::row`. The view holds no
//! data of its own beyond the scroll position and focus.

use std::cell::Cell;
use std::f32::consts::FRAC_PI_2;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use disk_atlas_lib::fs::{NodeId, NodeKind};
use disk_atlas_lib::mac::bundle;
use disk_atlas_lib::search::match_ranges;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollStrategy,
    SharedString, Subscription, Transformation, UniformListScrollHandle, Window, canvas, div,
    point, prelude::*, px, radians, svg, uniform_list,
};

use crate::assets::icon;
use crate::components::file_row::{
    FileRow, FolderCache, columns, folder_cell, kind_cell, modified_cell, name_cell, resource_cell,
    row_frame, size_cell,
};
use crate::components::icon_tile::IconTile;
use crate::components::menu::MenuSlot;
use crate::components::toolbar::FilterRow;
use crate::elevation::{self, Restart};
use crate::keymap::{
    BUTTON, FILE_LIST, RENAMING, context, controls, file_list, host_ops, menu as menu_keys,
    quick_look,
};
use crate::ops::{self, Conflict, HostPath, Step, UndoStack};
use crate::state::icon_grid;
use crate::state::listing::{Click, Listing, ListingEvent, LoadState, Motion};
use crate::state::menus::{
    BUILT, MAX_ROW_FACTS, MenuState, MenuTarget, RowFacts, build, file_manager_name,
};
use crate::state::order::{SortColumn, SortSpec};
use crate::state::rename::RenameEdit;
use crate::state::search::{Search, SearchEvent};
use crate::state::sources::{Availability, SourceId};
use crate::state::thumbnails::{self, Key, Origin, Thumbnails, Want};
use crate::state::view::{ViewEvent, ViewMode, ViewState};
use crate::theme::{
    Colors, Theme, metrics,
    motion::{self, Entrance, Tween},
    scale::{Density, radius, size, space, text},
};

/// How long a pause ends the name being typed: a keystroke later than this starts a new one.
const TYPE_TO_SELECT_TIMEOUT: Duration = Duration::from_secs(1);

/// The selection plate gliding from row `from` to row `to` after a one-step keyboard move.
#[derive(Debug, Clone, Copy)]
struct Glide {
    from: usize,
    to: usize,
    tween: Tween,
}

/// The new folder's listing coming in (`r1-redesign.md` 2.2).
#[derive(Debug, Clone, Copy)]
enum Slide {
    /// The folder changed and nothing of it is drawn yet: the slide starts with its first rows
    /// (or its state message), from `offset` pixels away.
    Waiting { offset: f32 },
    /// Sliding in.
    Running(Entrance),
}

/// Where the new listing slides in from: the right when going deeper or sideways, the left when
/// going up towards the root. Depths are crumb counts.
fn slide_offset(old_depth: usize, new_depth: usize) -> f32 {
    if new_depth < old_depth {
        -motion::FOLDER_SLIDE
    } else {
        motion::FOLDER_SLIDE
    }
}

/// The list position under the pointer at `pointer`, given the list's `bounds` and vertical
/// scroll offset (zero or negative) as last laid out. `None` outside the list or past its rows.
fn row_under(
    pointer: Point<Pixels>,
    bounds: Bounds<Pixels>,
    scroll_y: Pixels,
    row_height: Pixels,
    count: usize,
) -> Option<usize> {
    if !bounds.contains(&pointer) || row_height <= Pixels::ZERO {
        return None;
    }
    let content_y = f32::from(pointer.y - bounds.origin.y - scroll_y);
    if content_y < 0.0 {
        return None;
    }
    let position = (content_y / f32::from(row_height)) as usize;
    (position < count).then_some(position)
}

/// True when the row at `position` is wholly inside a list `height` tall scrolled by `scroll_y`.
fn row_fully_visible(
    position: usize,
    scroll_y: Pixels,
    height: Pixels,
    row_height: Pixels,
) -> bool {
    let top = row_height * position as f32 + scroll_y;
    top >= Pixels::ZERO && top + row_height <= height
}

/// The text typed so far for type-to-select, and when the last character arrived.
///
/// Plain data: the caller passes the current time in and a closure that says whether a prefix
/// matches a row, so the rules can be tested without a window or a clock.
#[derive(Debug, Default)]
struct TypeBuffer {
    text: String,
    last: Option<Instant>,
}

impl TypeBuffer {
    /// True when a name is being typed: the buffer is not empty and the last keystroke was at
    /// most [`TYPE_TO_SELECT_TIMEOUT`] before `now`.
    fn is_active(&self, now: Instant) -> bool {
        !self.text.is_empty()
            && self
                .last
                .is_some_and(|last| now.saturating_duration_since(last) <= TYPE_TO_SELECT_TIMEOUT)
    }

    /// Forgets what was typed.
    fn clear(&mut self) {
        self.text.clear();
        self.last = None;
    }

    /// Adds `ch` typed at `now` and asks `matches` whether the buffer now names a row (the
    /// closure also moves the cursor there). When it does not and the buffer holds more than
    /// `ch`, starts again with `ch` alone and tries once more.
    ///
    /// Returns false when the key was ignored (a space with no name being typed) or nothing
    /// matched.
    fn type_char(&mut self, ch: char, now: Instant, mut matches: impl FnMut(&str) -> bool) -> bool {
        let active = self.is_active(now);
        // Space is reserved for Quick Look unless it continues a name.
        if ch == ' ' && !active {
            return false;
        }
        if !active {
            self.text.clear();
        }
        self.text.push(ch);
        self.last = Some(now);
        if matches(&self.text) {
            return true;
        }
        if self.text.chars().nth(1).is_some() {
            self.text.clear();
            self.text.push(ch);
            return matches(&self.text);
        }
        false
    }
}

/// The sort a click on the header label of `clicked` asks for.
///
/// In a folder the same column flips direction and another column starts ascending. In search
/// results the same column goes ascending, then descending, then back to `Relevance`, which
/// shows no arrow.
fn next_sort(current: SortSpec, clicked: SortColumn, in_results: bool) -> SortSpec {
    let ascending = |column| SortSpec {
        column,
        descending: false,
    };
    if current.column != clicked || clicked == SortColumn::Relevance {
        return ascending(clicked);
    }
    match (in_results, current.descending) {
        (true, true) => ascending(SortColumn::Relevance),
        (_, descending) => SortSpec {
            column: clicked,
            descending: !descending,
        },
    }
}

/// What the list says when a search found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoResults {
    /// The first line.
    message: String,
    /// True when filters are set: a second line and the "Clear filters" button follow.
    filters_on: bool,
}

impl NoResults {
    /// `query` is the text typed; `has_terms` says whether it held any word to look for (a query
    /// of spaces has none, and only the filters are narrowing).
    fn new(query: &str, has_terms: bool, filters_on: bool) -> Self {
        let message = if has_terms {
            format!("No names contain \u{201c}{}\u{201d}.", query.trim())
        } else {
            "Nothing matches these filters.".to_owned()
        };
        Self {
            message,
            filters_on,
        }
    }
}

/// A state's words (`ui-design.md` 1.8): what happened in the heading style, and what to do next
/// or where it happened under it in `ink3`.
fn state_message(title: SharedString, detail: Option<SharedString>, colors: &Colors) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(space::S4)
        .text_center()
        .child(
            div()
                .text_size(text::HEADING.size)
                .font_weight(text::HEADING.weight)
                .text_color(colors.ink2)
                .child(title),
        )
        .children(detail.map(|detail| div().text_color(colors.ink3).child(detail)))
}

/// The empty-folder detail: where nothing is stored.
fn empty_folder_detail(folder: &str, volume: &str, at_root: bool) -> String {
    if at_root {
        format!("Nothing is stored on {volume}.")
    } else {
        format!("Nothing is stored in {folder} on {volume}.")
    }
}

/// What a source that needs permission says (`ui-design.md` 1.8). Windows has its own reason.
fn permission_detail() -> &'static str {
    if cfg!(windows) {
        "Windows lets only administrators read a whole disk. Disk Atlas asks once, through the Windows prompt, and only ever reads."
    } else {
        "Reading a whole disk needs administrator rights. Disk Atlas only ever reads."
    }
}

/// What a "Copy as" action puts on the clipboard for each selected row.
#[derive(Debug, Clone, Copy)]
enum CopyKind {
    Name,
    Path,
    MacPath,
    NodeId,
}

/// A disk image or sparse bundle by its name: what "Open in Disk Atlas" can take.
fn is_disk_image(name: &str, folder: bool) -> bool {
    if folder {
        name.to_ascii_lowercase().ends_with(".sparsebundle")
    } else {
        crate::format::kind(name, disk_atlas_lib::fs::NodeKind::File) == "Disk image"
    }
}

/// The text a "Copy as" puts on the clipboard for the row at `position`. On the host a path is
/// the real one; elsewhere it starts with the volume's name, `/` between names (`:` for the Mac
/// form), as the breadcrumb and the Folder column of results show it.
/// Copies or moves `items` into `dir`, one by one; the first failure stops the rest.
fn paste_job(items: &[HostPath], dir: &HostPath, cut: bool, conflict: Conflict) -> OpOutcome {
    let mut outcome = OpOutcome {
        spent_clipboard: cut,
        ..OpOutcome::default()
    };
    let mut created = Vec::new();
    for item in items {
        let done = if cut {
            ops::move_into(item, dir, conflict)
        } else {
            ops::copy_into(item, dir, conflict)
        };
        match done {
            Ok(Some(to)) => {
                if cut {
                    outcome.steps.push(Step::Moved {
                        from: item.path().to_path_buf(),
                        to: to.clone(),
                    });
                }
                created.push(to);
            }
            Ok(None) => {}
            Err(error) => {
                outcome.error = Some(error.to_string());
                break;
            }
        }
    }
    if !cut && !created.is_empty() {
        outcome.steps.push(Step::Copied {
            created: created.clone(),
        });
    }
    outcome.select = created
        .first()
        .and_then(|path| path.file_name())
        .map(|name| (name.to_string_lossy().into_owned(), false));
    outcome
}

/// The thumbnail of `row` when it is a picture and the listing is a plain folder; a miss is
/// added to `wants` for the pipeline.
fn thumbnail_of(
    listing: &Listing,
    thumbnails: &Thumbnails,
    row: &crate::state::listing::Row<'_>,
    edge: u32,
    wants: &mut Vec<Want>,
) -> Option<std::sync::Arc<gpui::RenderImage>> {
    let entry = &row.entry;
    if listing.in_results() {
        return None;
    }
    let origin = if thumbnails::eligible(entry.name, entry.kind, entry.size) {
        Origin::Picture
    } else if entry.kind == NodeKind::Directory {
        if bundle::is_package(entry.name, entry.kind, entry.flags) {
            Origin::PackageIcon
        } else {
            Origin::FolderIcon
        }
    } else {
        return None;
    };
    let location = listing.location()?;
    let key = Key {
        source: location.source(),
        node: entry.node,
        edge,
        modified: entry.modified.map(|time| time.unix_nanos()),
    };
    wants.push(Want {
        key,
        fs: std::sync::Arc::clone(location.fs()),
        name: SharedString::from(entry.name.to_owned()),
        origin,
    });
    thumbnails.image(&key)
}

fn row_text(listing: &Listing, position: usize, kind: CopyKind, cx: &App) -> Option<String> {
    let row = listing.row(position)?;
    let (separator, mac) = match kind {
        CopyKind::Name => return Some(row.entry.name.to_owned()),
        CopyKind::NodeId => return Some(row.entry.node.0.to_string()),
        CopyKind::Path => ('/', false),
        CopyKind::MacPath => (':', true),
    };
    if !mac && let Some(path) = listing.host_path_of(position, cx) {
        return Some(path.display().to_string());
    }
    let location = listing.location()?;
    let mut text = match listing.folder_of(position) {
        Some(folder) if !folder.is_empty() => folder.to_string(),
        Some(_) => location.crumbs().first()?.name.to_string(),
        None => location
            .crumbs()
            .iter()
            .map(|crumb| crumb.name.as_ref())
            .collect::<Vec<_>>()
            .join("/"),
    };
    if mac {
        text = text.replace('/', ":");
    }
    text.push(separator);
    text.push_str(row.entry.name);
    Some(text)
}

/// The file list view.
/// Most items one operation takes.
const MAX_OP_ITEMS: usize = 10_000;

/// A name being edited in place.
struct Renaming {
    path: HostPath,
    position: usize,
    edit: RenameEdit,
}

/// Items remembered by Copy or Cut, for Paste.
struct OpsClipboard {
    items: Vec<HostPath>,
    cut: bool,
}

/// What a finished operation tells the explorer.
#[derive(Default)]
struct OpOutcome {
    /// Steps to remember for undo, oldest first.
    steps: Vec<Step>,
    /// A sentence for the user when something failed.
    error: Option<String>,
    /// A name to select once the folder is read again, and whether to start renaming it.
    select: Option<(String, bool)>,
    /// Paste: the items were moved, so the remembered ones are used up.
    spent_clipboard: bool,
}

pub struct Explorer {
    listing: Entity<Listing>,
    /// Whether the folder is drawn as a list or as tiles.
    view: Entity<ViewState>,
    /// The name being edited in place.
    renaming: Option<Renaming>,
    /// What Ctrl+Z takes back.
    undo: UndoStack,
    /// True while a confirmation prompt waits for an answer; Ctrl+Z does nothing then.
    prompt_open: bool,
    /// What Ctrl+C or Ctrl+X remembered.
    ops_clipboard: Option<OpsClipboard>,
    /// The last operation's failure, shown under the list until the next action.
    ops_notice: Option<SharedString>,
    /// A name to select (and start renaming) when the folder has been read again.
    pending_select: Option<(String, bool)>,
    /// Pictures' thumbnails, for tiles and list rows.
    thumbnails: Entity<Thumbnails>,
    /// Width of the area the tiles fill, as the last frame measured it.
    grid_width: Rc<Cell<Pixels>>,
    /// Tiles across in the icon view, from `grid_width`.
    grid_columns: usize,
    search: Entity<Search>,
    /// The filter controls above the column header, shown while a search is active.
    filter_row: Entity<FilterRow>,
    /// Byte ranges of the name being drawn that the search terms matched, reused row to row.
    ranges: Vec<Range<usize>>,
    /// The width of the Folder column as the header last laid it out, read by the rows of the
    /// same frame: the header is prepainted before the list.
    folder_width: Rc<Cell<Pixels>>,
    /// Folder paths cut to `folder_width`, for the visible result rows.
    folder_cache: FolderCache,
    /// Focus of the "Clear filters" button, a tab stop while it is shown.
    clear_focus: FocusHandle,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// The folder the scroll position belongs to.
    shown: Option<(SourceId, NodeId)>,
    /// What the user has typed to jump to a row.
    typed: TypeBuffer,
    /// Focus of the "Restart as administrator" button, a tab stop while it is shown.
    restart_focus: FocusHandle,
    /// True from the click on "Restart as administrator" until the prompt is answered, so a
    /// second click does not raise a second prompt.
    restarting: bool,
    /// The context menu, while one is open.
    menu: MenuSlot,
    /// The row the pointer is over and when it got there, for the hover fade. Only this row is
    /// animated, however long the list.
    hover: Option<(usize, Instant)>,
    /// How much hover fill `render_rows` draws on which row this frame.
    hover_paint: Option<(usize, f32)>,
    /// The selection plate, while it glides.
    glide: Option<Glide>,
    /// The row whose selection fill the plate draws this frame, and the plate's top in list
    /// content coordinates.
    glide_paint: Option<(usize, Pixels)>,
    /// When the last one-step keyboard move happened, to tell a held key from a press.
    last_step: Option<Instant>,
    /// Crumb count of the folder shown, for the direction of the next slide.
    shown_depth: usize,
    /// The new folder's listing coming in.
    slide: Option<Slide>,
    _subscriptions: Vec<Subscription>,
}

impl Explorer {
    /// Shows the rows of `listing`, with the filter controls of `filter_row` above them while
    /// `search` is active.
    pub fn new(
        listing: Entity<Listing>,
        view: Entity<ViewState>,
        thumbnails: Entity<Thumbnails>,
        search: Entity<Search>,
        filter_row: Entity<FilterRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, _: &SearchEvent, cx| cx.notify()),
            cx.observe(&thumbnails, |_, _, cx| cx.notify()),
            cx.subscribe(&view, |this, _, _: &ViewEvent, cx| {
                // The other layout has other row heights: start at the top.
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }),
            cx.observe_in(&listing, window, |this, listing, window, cx| {
                // A menu belongs to the folder it was opened in.
                if this.follow_folder(&listing, cx) {
                    this.menu.dismiss(window, cx);
                    this.renaming = None;
                }
                this.apply_pending_select(&listing, window, cx);
                cx.notify();
            }),
            cx.subscribe(
                &listing,
                |this, _listing, event: &ListingEvent, cx| match event {
                    ListingEvent::CursorMoved => this.reveal_cursor(cx),
                    // `Search` resets on this; the list has nothing to do.
                    ListingEvent::ResultsDropped => {}
                },
            ),
        ];
        Self {
            listing,
            view,
            renaming: None,
            undo: UndoStack::default(),
            prompt_open: false,
            ops_clipboard: None,
            ops_notice: None,
            pending_select: None,
            thumbnails,
            grid_width: Rc::new(Cell::new(Pixels::ZERO)),
            grid_columns: icon_grid::UNMEASURED_COLUMNS,
            search,
            filter_row,
            ranges: Vec::new(),
            folder_width: Rc::new(Cell::new(Pixels::ZERO)),
            folder_cache: FolderCache::default(),
            clear_focus: cx.focus_handle().tab_stop(true),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            shown: None,
            typed: TypeBuffer::default(),
            restart_focus: cx.focus_handle().tab_stop(true),
            restarting: false,
            menu: MenuSlot::default(),
            hover: None,
            hover_paint: None,
            glide: None,
            glide_paint: None,
            last_step: None,
            shown_depth: 0,
            slide: None,
            _subscriptions: subscriptions,
        }
    }

    /// A new folder starts at the top. True when the folder changed.
    fn follow_folder(&mut self, listing: &Entity<Listing>, cx: &App) -> bool {
        let location = listing.read(cx).location();
        let shown = location.map(|location| (location.source(), location.node()));
        let depth = location.map_or(0, |location| location.crumbs().len());
        if shown != self.shown {
            // The first folder of the window appears as it is; every later one slides in.
            self.slide = self.shown.and(shown).map(|_| Slide::Waiting {
                offset: slide_offset(self.shown_depth, depth),
            });
            self.shown = shown;
            self.shown_depth = depth;
            self.hover = None;
            self.glide = None;
            self.typed.clear();
            self.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(Pixels::ZERO, Pixels::ZERO));
            return true;
        }
        false
    }

    /// True while the folder is drawn as tiles.
    fn icons(&self, cx: &App) -> bool {
        self.view.read(cx).mode() == ViewMode::Icons
    }

    /// Side of a tile: the roomy density draws larger ones.
    fn tile_side() -> f32 {
        match Density::default() {
            Density::Roomy => icon_grid::TILE_ROOMY,
            Density::Compact | Density::Regular => icon_grid::TILE_REGULAR,
        }
    }

    /// Height of one scrolled item: a list row, or a row of tiles.
    fn item_height(&self, cx: &App) -> Pixels {
        if self.icons(cx) {
            px(icon_grid::row_height(Self::tile_side()))
        } else {
            Density::default().row_height()
        }
    }

    /// The scrolled item that holds list `position`.
    fn item_of(&self, position: usize, cx: &App) -> usize {
        if self.icons(cx) {
            icon_grid::row_of(position, self.grid_columns)
        } else {
            position
        }
    }

    /// Scrolls the cursor row into view with the smallest movement: nothing if it is fully
    /// visible, otherwise to the nearer edge.
    fn reveal_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(position) = self.listing.read(cx).cursor() else {
            return;
        };
        let item = self.item_of(position, cx);
        let item_height = self.item_height(cx);
        let above = {
            let state = self.scroll.0.borrow();
            let top = -state.base_handle.offset().y;
            item_height * item < top
        };
        // A non-strict scroll does nothing for a fully visible row; for one out of view, the
        // strategy names the edge it lands on.
        let strategy = if above {
            ScrollStrategy::Top
        } else {
            ScrollStrategy::Bottom
        };
        self.scroll.scroll_to_item(item, strategy);
        cx.notify();
    }

    /// Whole positions visible in the list, at least one: in the icon view, whole rows of tiles.
    fn page_rows(&self, cx: &App) -> usize {
        let height = self
            .scroll
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height);
        let rows = height.map_or(1, |height| {
            (height / self.item_height(cx)).floor().max(1.0) as usize
        });
        if self.icons(cx) {
            rows * self.grid_columns
        } else {
            rows
        }
    }

    fn move_cursor(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        let from = self.listing.read(cx).cursor();
        self.listing
            .update(cx, |listing, cx| listing.move_cursor(motion, extend, cx));
        let to = self.listing.read(cx).cursor();
        let one_step = !extend && matches!(motion, Motion::Up | Motion::Down) && !self.icons(cx);
        let now = Instant::now();
        self.glide = None;
        if one_step
            && let (Some(from), Some(to)) = (from, to)
            && from.abs_diff(to) == 1
            && motion::glides(self.last_step, now, motion::reduced(cx))
        {
            // The plate glides only between two rows already on screen; a move that scrolls is
            // shown at once.
            let (scroll_y, height) = {
                let state = self.scroll.0.borrow();
                (
                    state.base_handle.offset().y,
                    state.base_handle.bounds().size.height,
                )
            };
            let row_height = Density::default().row_height();
            if row_fully_visible(from, scroll_y, height, row_height)
                && row_fully_visible(to, scroll_y, height, row_height)
            {
                self.glide = Some(Glide {
                    from,
                    to,
                    tween: Tween::new(now, motion::QUICK),
                });
            }
        }
        self.last_step = one_step.then_some(now);
    }

    /// One step up (`forward == false`) or down: a row of the list, or a whole row of tiles.
    fn step_vertical(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        let motion = if !self.icons(cx) {
            if forward { Motion::Down } else { Motion::Up }
        } else if forward {
            Motion::PageDown(self.grid_columns)
        } else {
            Motion::PageUp(self.grid_columns)
        };
        self.move_cursor(motion, extend, cx);
    }

    /// One tile left or right. Does nothing in the list, where the keys are not bound.
    fn step_across(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        if !self.icons(cx) {
            cx.propagate();
            return;
        }
        self.move_cursor(if forward { Motion::Down } else { Motion::Up }, extend, cx);
    }

    fn move_up(&mut self, _: &file_list::MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_vertical(false, false, cx);
    }

    fn move_down(&mut self, _: &file_list::MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_vertical(true, false, cx);
    }

    fn move_left(&mut self, _: &file_list::MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.step_across(false, false, cx);
    }

    fn move_right(&mut self, _: &file_list::MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.step_across(true, false, cx);
    }

    fn select_left(&mut self, _: &file_list::SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.step_across(false, true, cx);
    }

    fn select_right(&mut self, _: &file_list::SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.step_across(true, true, cx);
    }

    fn page_up(&mut self, _: &file_list::PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Motion::PageUp(self.page_rows(cx)), false, cx);
    }

    fn page_down(&mut self, _: &file_list::PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Motion::PageDown(self.page_rows(cx)), false, cx);
    }

    fn move_to_first(
        &mut self,
        _: &file_list::MoveToFirst,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(Motion::First, false, cx);
    }

    fn move_to_last(&mut self, _: &file_list::MoveToLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(Motion::Last, false, cx);
    }

    fn select_up(&mut self, _: &file_list::SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_vertical(false, true, cx);
    }

    fn select_down(&mut self, _: &file_list::SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_vertical(true, true, cx);
    }

    fn select_page_up(
        &mut self,
        _: &file_list::SelectPageUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(Motion::PageUp(self.page_rows(cx)), true, cx);
    }

    fn select_page_down(
        &mut self,
        _: &file_list::SelectPageDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(Motion::PageDown(self.page_rows(cx)), true, cx);
    }

    fn select_to_first(
        &mut self,
        _: &file_list::SelectToFirst,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(Motion::First, true, cx);
    }

    fn select_to_last(
        &mut self,
        _: &file_list::SelectToLast,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_cursor(Motion::Last, true, cx);
    }

    /// Enter: a folder is opened; a file is left for the window, which opens Quick Look on it.
    fn open(&mut self, _: &file_list::Open, _: &mut Window, cx: &mut Context<Self>) {
        let on_file = {
            let listing = self.listing.read(cx);
            listing.cursor().is_some_and(|position| {
                listing
                    .row(position)
                    .is_some_and(|row| !row.entry.is_browsable())
                    || listing.is_package_row(position, cx)
            })
        };
        if on_file {
            cx.propagate();
        } else {
            self.listing
                .update(cx, |listing, cx| listing.activate_cursor(cx));
        }
    }

    /// Ctrl+Enter: in search results, shows the file under the cursor in its folder (what Enter
    /// did before it opened Quick Look). Nothing in a plain folder.
    fn show_in_folder(
        &mut self,
        _: &file_list::ShowInFolder,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.listing.read(cx).in_results() {
            self.listing
                .update(cx, |listing, cx| listing.activate_cursor(cx));
        }
    }

    /// "Show original": goes to what the link or alias under the cursor points at.
    fn show_original(
        &mut self,
        _: &file_list::ShowOriginal,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.listing
            .update(cx, |listing, cx| listing.show_original_cursor(cx));
    }

    /// "Show package contents": goes inside the package under the cursor.
    fn show_package_contents(
        &mut self,
        _: &file_list::ShowPackageContents,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.listing
            .update(cx, |listing, cx| listing.open_package_cursor(cx));
    }

    /// Shift+Enter: opens the file under the cursor in its own app, for files of this computer.
    fn open_in_app(&mut self, _: &file_list::OpenInApp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.listing.read(cx).host_path_of_cursor(cx) {
            cx.open_with_system(&path);
        }
    }

    /// Space: Quick Look, unless it continues a name being typed. The window handles it.
    fn toggle_quick_look(
        &mut self,
        _: &quick_look::Toggle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.typed.is_active(Instant::now()) {
            self.type_text(" ", cx);
        } else {
            cx.propagate();
        }
    }

    fn select_all(&mut self, _: &file_list::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.listing
            .update(cx, |listing, cx| listing.select_all(cx));
    }

    /// Type-to-select. Keys bound to actions never get here: GPUI runs the bindings first and
    /// only offers the key to `on_key_down` listeners when no action handled it.
    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        if self.renaming.is_some() {
            if !(modifiers.control || modifiers.alt || modifiers.platform)
                && let Some(text) = keystroke.key_char.as_deref()
            {
                self.edit(cx, |edit| edit.insert(text));
            }
            cx.stop_propagation();
            return;
        }
        // Shift is how capitals are typed; the rest make the key a shortcut, not text.
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let Some(text) = keystroke.key_char.as_deref() else {
            return;
        };
        if self.type_text(text, cx) {
            cx.stop_propagation();
        }
    }

    /// Feeds typed `text` to type-to-select. Returns true when the key was used.
    fn type_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let mut handled = false;
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            let listing = self.listing.clone();
            self.typed.type_char(ch, now, |prefix| {
                listing.update(cx, |listing, cx| listing.select_prefix(prefix, cx))
            });
            // A space that is not part of a name is left for Quick Look.
            handled |= ch != ' ' || self.typed.is_active(now);
        }
        handled
    }

    // ---- context menus ----

    /// Opens `target`'s menu at `at`. Nothing opens when the target holds nothing.
    fn show_menu(
        &mut self,
        target: &MenuTarget,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visibility = crate::state::visibility::Visibility::current(cx);
        let state = MenuState {
            built: BUILT,
            can_paste: false,
            sort: self.listing.read(cx).sort(),
            file_manager: file_manager_name(),
            show_hidden: visibility.show_hidden,
            show_system: visibility.show_system,
        };
        let items = build(target, &state);
        if items.is_empty() {
            self.menu.clear();
            return;
        }
        let focus = self.focus.clone();
        self.menu
            .show(items, at, Some(focus), window, cx, |this, _, _, cx| {
                this.menu.clear();
                cx.notify();
            });
    }

    /// The target for the selected rows, when any are selected.
    fn rows_target(&self, cx: &App) -> Option<MenuTarget> {
        let listing = self.listing.read(cx);
        let source = listing.source_facts(cx)?;
        let in_search = listing.in_results();
        let rows: Vec<RowFacts> = listing
            .selected_positions(MAX_ROW_FACTS)
            .into_iter()
            .filter_map(|position| listing.row(position).map(|row| (position, row)))
            .map(|(position, row)| {
                let folder = row.entry.is_browsable();
                RowFacts {
                    folder,
                    disk_image: is_disk_image(row.entry.name, folder),
                    package: listing.is_package_row(position, cx),
                    original: listing.is_link_row(position),
                }
            })
            .collect();
        (!rows.is_empty()).then_some(MenuTarget::Rows {
            source,
            rows,
            in_search,
        })
    }

    /// A right press on the row at `position`: a row that is not selected is selected first; one
    /// that is selected keeps the selection it is part of.
    fn right_press_row(
        &mut self,
        position: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let selected = self
            .listing
            .read(cx)
            .row(position)
            .is_some_and(|row| row.selected);
        if !selected {
            self.listing.update(cx, |listing, cx| {
                listing.click(position, Click::Replace, cx)
            });
        }
        if let Some(target) = self.rows_target(cx) {
            self.show_menu(&target, event.position, window, cx);
        }
    }

    /// A right press on empty space: the selection clears and the folder's menu opens.
    fn right_press_empty(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.listing
            .update(cx, |listing, cx| listing.clear_selection(cx));
        let Some(source) = self.listing.read(cx).source_facts(cx) else {
            return;
        };
        let in_search = self.listing.read(cx).in_results();
        self.show_menu(&MenuTarget::Folder { source, in_search }, at, window, cx);
    }

    /// A right press on the column header.
    fn right_press_header(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let listing = self.listing.read(cx);
        let target = MenuTarget::ColumnHeader {
            in_search: listing.in_results(),
            resource_fork: listing.resource_forks(),
        };
        self.show_menu(&target, at, window, cx);
    }

    /// The Menu key and Shift+F10: the menu of the cursor row, or of the folder when the list
    /// has no cursor.
    fn open_menu(&mut self, _: &menu_keys::Open, window: &mut Window, cx: &mut Context<Self>) {
        let (cursor, selected) = {
            let listing = self.listing.read(cx);
            (listing.cursor(), listing.selected_count())
        };
        let Some(position) = cursor else {
            let at = self.row_anchor(0, self.icons(cx));
            self.right_press_empty(at, window, cx);
            return;
        };
        if selected == 0 {
            self.listing.update(cx, |listing, cx| {
                listing.click(position, Click::Replace, cx)
            });
        }
        let at = self.row_anchor(position, self.icons(cx));
        if let Some(target) = self.rows_target(cx) {
            self.show_menu(&target, at, window, cx);
        }
    }

    /// Where a menu opened from the keyboard goes: inside the name column, under the row's
    /// middle, so the menu does not cover the row.
    fn row_anchor(&self, position: usize, icons: bool) -> Point<Pixels> {
        let state = self.scroll.0.borrow();
        let bounds = state.base_handle.bounds();
        let offset = state.base_handle.offset();
        if icons {
            let side = Self::tile_side();
            let columns = self.grid_columns.max(1);
            let (row, column) = (position / columns, position % columns);
            return point(
                bounds.left()
                    + px(icon_grid::INSET + (side + icon_grid::GAP) * column as f32 + side / 2.0),
                bounds.top() + offset.y + px(icon_grid::row_height(side)) * row as f32 + px(side),
            );
        }
        let row = Density::default().row_height();
        point(
            bounds.left() + space::S48,
            bounds.top() + offset.y + row * position + row,
        )
    }

    fn sort_column(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        self.listing.update(cx, |listing, cx| {
            let spec = SortSpec {
                column,
                descending: listing.sort().descending,
            };
            listing.set_sort(spec, cx);
        });
    }

    fn sort_direction(&mut self, descending: bool, cx: &mut Context<Self>) {
        self.listing.update(cx, |listing, cx| {
            let spec = SortSpec {
                descending,
                ..listing.sort()
            };
            listing.set_sort(spec, cx);
        });
    }

    /// Puts one line per selected row on the clipboard.
    fn copy_rows(&mut self, kind: CopyKind, cx: &mut Context<Self>) {
        let listing = self.listing.read(cx);
        let lines: Vec<String> = listing
            .selected_positions(usize::MAX)
            .into_iter()
            .filter_map(|position| row_text(listing, position, kind, cx))
            .collect();
        if !lines.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(lines.join("\n")));
        }
    }

    fn copy_folder_path(
        &mut self,
        _: &context::CopyFolderPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let listing = self.listing.read(cx);
        let text = listing.host_folder_path(cx).map_or_else(
            || {
                listing.location().map(|location| {
                    let names: Vec<&str> = location
                        .crumbs()
                        .iter()
                        .map(|crumb| crumb.name.as_ref())
                        .collect();
                    names.join("/")
                })
            },
            |path| Some(path.display().to_string()),
        );
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// "Show in Explorer": the selected row, or the folder when nothing is selected.
    fn show_in_file_manager(
        &mut self,
        _: &context::ShowInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let listing = self.listing.read(cx);
        let path = match listing.selected_positions(1).first() {
            Some(&position) => listing.host_path_of(position, cx),
            None => listing.host_folder_path(cx),
        };
        if let Some(path) = path {
            cx.reveal_path(&path);
        }
    }

    /// A left press on the row at `position`.
    fn press_row(&mut self, position: usize, event: &MouseDownEvent, cx: &mut Context<Self>) {
        // The empty area below the rows clears the selection; a row press must not reach it.
        cx.stop_propagation();
        let modifiers = event.modifiers;
        if event.click_count >= 2 && !modifiers.shift && !modifiers.secondary() {
            self.listing
                .update(cx, |listing, cx| listing.activate(position, cx));
            return;
        }
        let click = if modifiers.shift {
            Click::Extend
        } else if modifiers.secondary() {
            Click::Toggle
        } else {
            Click::Replace
        };
        self.listing
            .update(cx, |listing, cx| listing.click(position, click, cx));
    }

    /// The rows of the visible `range`. Exactly one element per position.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let end = range.end;
        let range_for_visible = range.clone();
        let listing = self.listing.read(cx);
        let resource_column = listing.resource_forks();
        let in_results = listing.in_results();
        if !in_results {
            self.folder_cache.clear();
        }
        let folder_width = self.folder_width.get();
        let thumbs = self.thumbnails.read(cx);
        let mut wants = Vec::new();
        let edge = thumbnails::edge_for(f32::from(Density::default().icon_size()));
        let rows = range
            .map(|position| match listing.row(position) {
                Some(row) => {
                    let hover = self
                        .hover_paint
                        .and_then(|(at, amount)| (at == position).then_some(amount));
                    let under_plate = self.glide_paint.is_some_and(|(to, _)| to == position);
                    let mut file_row =
                        FileRow::new(&row, position, resource_column, listing.local_times())
                            .system(listing.system_of(&row))
                            .thumbnail(thumbnail_of(listing, thumbs, &row, edge, &mut wants))
                            .hover(hover)
                            .under_plate(under_plate)
                            .editing(self.edit_of(position));
                    if in_results {
                        match_ranges(row.entry.name, listing.terms(), &mut self.ranges);
                        let folder = listing.folder_of(position).unwrap_or_default();
                        // The end of a path tells two results apart: cut from the middle.
                        let folder = self.folder_cache.fit(window, folder, folder_width);
                        file_row = file_row.in_results(folder, self.ranges.clone());
                    }
                    file_row
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            this.hover_row(position, *hovered, cx);
                        }))
                        .on_mouse_down(cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.press_row(position, event, cx);
                        }))
                        .on_context_menu(cx.listener(
                            move |this, event: &MouseDownEvent, window, cx| {
                                this.right_press_row(position, event, window, cx);
                            },
                        ))
                        .into_any_element()
                }
                None => row_frame().into_any_element(),
            })
            .collect();
        let thumbnails = self.thumbnails.clone();
        cx.defer(move |cx| thumbnails.update(cx, |t, cx| t.want(wants, cx)));
        if in_results {
            // Sorting the rest of a large result set starts work; keep that out of the paint.
            let listing = self.listing.clone();
            cx.defer(move |cx| {
                listing.update(cx, |listing, cx| listing.ensure_sorted_through(end, cx));
            });
        }
        // The folder rows on screen are measured first (host sizes); the listing waits for
        // scrolling to settle before it asks, and does the asking off the paint.
        let listing = self.listing.clone();
        let visible = range_for_visible;
        cx.defer(move |cx| {
            listing.update(cx, |listing, cx| listing.note_visible(visible, cx));
        });
        rows
    }

    // ---------------------------------------------------------------------------------------
    // Host file operations (R8)
    // ---------------------------------------------------------------------------------------

    /// The selected rows as host paths; empty in a folder that is not the host's.
    fn selected_host_paths(&self, cx: &App) -> Vec<HostPath> {
        let listing = self.listing.read(cx);
        listing
            .selected_positions(MAX_OP_ITEMS)
            .into_iter()
            .filter_map(|position| HostPath::of(listing, position, cx))
            .collect()
    }

    /// Refuses (with a notice) when an item is, or holds, a disk or image that is open.
    fn guard_open(&mut self, items: &[HostPath], cx: &mut Context<Self>) -> bool {
        let open = self.listing.read(cx).open_disk_paths(cx);
        match ops::open_conflict(items, &open) {
            Some(item) => {
                let name = item
                    .path()
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.ops_notice = Some(
                    format!("\u{201c}{name}\u{201d} is open in Disk Atlas. Close it first.").into(),
                );
                cx.notify();
                false
            }
            None => true,
        }
    }

    /// Runs `job` on the background executor and applies its outcome here.
    fn run_op(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> OpOutcome + Send + 'static,
    ) {
        self.ops_notice = None;
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { job() }).await;
            this.update_in(cx, |this, _, cx| this.finish_op(outcome, cx))
                .ok();
        })
        .detach();
    }

    fn finish_op(&mut self, outcome: OpOutcome, cx: &mut Context<Self>) {
        for step in outcome.steps {
            self.undo.push(step);
        }
        if outcome.spent_clipboard {
            self.ops_clipboard = None;
        }
        self.ops_notice = outcome.error.map(SharedString::from);
        self.pending_select = outcome.select;
        self.listing.update(cx, |listing, cx| listing.refresh(cx));
        cx.notify();
    }

    /// After a re-read: selects the name an operation asked for, and starts renaming it when
    /// that was asked.
    fn apply_pending_select(
        &mut self,
        listing: &Entity<Listing>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((name, rename)) = self.pending_select.clone() else {
            return;
        };
        if !matches!(listing.read(cx).state(), LoadState::Ready) {
            return;
        }
        self.pending_select = None;
        let selected = listing.update(cx, |listing, cx| listing.select_name(&name, cx));
        if selected && rename {
            self.start_rename(window, cx);
        }
    }

    /// F2: edits the name of the row under the cursor in place.
    fn rename(&mut self, _: &host_ops::Rename, window: &mut Window, cx: &mut Context<Self>) {
        self.start_rename(window, cx);
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (position, path, name) = {
            let listing = self.listing.read(cx);
            let Some(position) = listing.cursor() else {
                return;
            };
            let (Some(path), Some(row)) =
                (HostPath::of(listing, position, cx), listing.row(position))
            else {
                return;
            };
            (position, path, row.entry.name.to_owned())
        };
        if !self.guard_open(std::slice::from_ref(&path), cx) {
            return;
        }
        self.ops_notice = None;
        self.renaming = Some(Renaming {
            path,
            position,
            edit: RenameEdit::new(&name),
        });
        window.focus(&self.focus);
        self.reveal_cursor(cx);
        cx.notify();
    }

    /// The name editor when `position` is the row being renamed.
    fn edit_of(&self, position: usize) -> Option<RenameEdit> {
        self.renaming
            .as_ref()
            .filter(|renaming| renaming.position == position)
            .map(|renaming| renaming.edit.clone())
    }

    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut RenameEdit)) {
        if let Some(renaming) = &mut self.renaming {
            change(&mut renaming.edit);
            cx.notify();
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        cx.notify();
        let name = renaming.edit.text().to_owned();
        let unchanged = renaming
            .path
            .path()
            .file_name()
            .is_some_and(|old| old.to_string_lossy() == name);
        if unchanged {
            return;
        }
        self.run_op(window, cx, move || {
            match ops::rename(&renaming.path, &name) {
                Ok(to) => OpOutcome {
                    steps: vec![Step::Moved {
                        from: renaming.path.path().to_path_buf(),
                        to,
                    }],
                    select: Some((name, false)),
                    ..OpOutcome::default()
                },
                Err(error) => OpOutcome {
                    error: Some(error.to_string()),
                    ..OpOutcome::default()
                },
            }
        });
    }

    /// Ctrl+Shift+N: a new folder in the folder shown, named in place.
    fn new_folder(&mut self, _: &host_ops::NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.listing.read(cx).in_results() {
            return;
        }
        let Some(parent) = HostPath::folder_of(self.listing.read(cx), cx) else {
            return;
        };
        self.run_op(window, cx, move || match ops::new_folder(&parent) {
            Ok(path) => OpOutcome {
                select: path
                    .file_name()
                    .map(|name| (name.to_string_lossy().into_owned(), true)),
                steps: vec![Step::NewFolder { path }],
                ..OpOutcome::default()
            },
            Err(error) => OpOutcome {
                error: Some(format!("Could not make a new folder: {error}")),
                ..OpOutcome::default()
            },
        });
    }

    /// Delete: asks, then moves the selected items to the Recycle Bin.
    fn move_to_trash(
        &mut self,
        _: &host_ops::MoveToTrash,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = self.selected_host_paths(cx);
        if items.is_empty() || !self.guard_open(&items, cx) {
            return;
        }
        let bin = if cfg!(target_os = "windows") {
            "Recycle Bin"
        } else {
            "Trash"
        };
        let question = match items.as_slice() {
            [one] => format!(
                "Move \u{201c}{}\u{201d} to the {bin}?",
                one.path()
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ),
            many => format!("Move {} items to the {bin}?", many.len()),
        };
        let move_label = format!("Move to the {bin}");
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &question,
            Some("You can put them back with Ctrl+Z, or from the bin."),
            &[move_label.as_str(), "Cancel"],
            cx,
        );
        self.prompt_open = true;
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            this.update(cx, |this, _| this.prompt_open = false).ok();
            if answer != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.run_op(window, cx, move || match ops::trash(&items) {
                    Ok(()) => OpOutcome {
                        steps: vec![Step::Trashed {
                            originals: items.iter().map(|item| item.path().to_path_buf()).collect(),
                        }],
                        ..OpOutcome::default()
                    },
                    Err(error) => OpOutcome {
                        error: Some(format!("Could not move to the bin: {error}")),
                        ..OpOutcome::default()
                    },
                });
            })
            .ok();
        })
        .detach();
    }

    /// Ctrl+Z: takes back the last operation.
    fn undo(&mut self, _: &host_ops::Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompt_open {
            return;
        }
        let Some(step) = self.undo.pop() else {
            return;
        };
        self.run_op(window, cx, move || match step.undo() {
            Ok(()) => OpOutcome::default(),
            Err(error) => OpOutcome {
                error: Some(error),
                ..OpOutcome::default()
            },
        });
    }

    /// Ctrl+C and Ctrl+X: remember the selected items for Paste. Copy also puts their paths on
    /// the system clipboard as text.
    fn remember_items(&mut self, cut: bool, cx: &mut Context<Self>) {
        let items = self.selected_host_paths(cx);
        if items.is_empty() {
            return;
        }
        if !cut {
            let text = items
                .iter()
                .map(|item| item.path().display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        self.ops_clipboard = Some(OpsClipboard { items, cut });
        self.ops_notice = None;
        cx.notify();
    }

    /// Ctrl+V: copies or moves the remembered items into the folder shown. Names that are taken
    /// are asked about once for the whole batch.
    fn paste_items(
        &mut self,
        _: &host_ops::PasteItems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = HostPath::folder_of(self.listing.read(cx), cx) else {
            return;
        };
        let Some(clipboard) = self.ops_clipboard.as_ref() else {
            return;
        };
        let (items, cut) = (clipboard.items.clone(), clipboard.cut);
        if self.listing.read(cx).in_results() || !self.guard_open(&items, cx) {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let (counted_items, counted_dir) = (items.clone(), dir.clone());
            let taken = cx
                .background_executor()
                .spawn(async move { ops::count_conflicts(&counted_items, &counted_dir) })
                .await;
            let conflict = if taken == 0 {
                Conflict::KeepBoth
            } else {
                let question = if taken == 1 {
                    "An item with that name is already here.".to_owned()
                } else {
                    format!("{taken} items with those names are already here.")
                };
                let answer = this
                    .update_in(cx, |this, window, cx| {
                        this.prompt_open = true;
                        window.prompt(
                            gpui::PromptLevel::Warning,
                            &question,
                            None,
                            &["Replace", "Keep both", "Skip", "Cancel"],
                            cx,
                        )
                    })
                    .ok();
                let Some(answer) = answer else {
                    return;
                };
                let result = answer.await;
                this.update(cx, |this, _| this.prompt_open = false).ok();
                match result {
                    Ok(0) => Conflict::Replace,
                    Ok(1) => Conflict::KeepBoth,
                    Ok(2) => Conflict::Skip,
                    _ => return,
                }
            };
            this.update_in(cx, |this, window, cx| {
                this.run_op(window, cx, move || paste_job(&items, &dir, cut, conflict));
            })
            .ok();
        })
        .detach();
    }

    /// The rows of tiles in the visible `range`: one element per grid row.
    fn render_tiles(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let columns = self.grid_columns;
        let side = Self::tile_side();
        let listing = self.listing.read(cx);
        let thumbs = self.thumbnails.read(cx);
        let mut wants = Vec::new();
        let edge = thumbnails::edge_for(side - 16.0);
        let len = listing.len();
        let rows = range
            .clone()
            .map(|grid_row| {
                let tiles = icon_grid::positions(grid_row, columns, len)
                    .filter_map(|position| {
                        let row = listing.row(position)?;
                        Some(
                            IconTile::new(&row, position, px(side))
                                .thumbnail(thumbnail_of(listing, thumbs, &row, edge, &mut wants))
                                .system(listing.system_of(&row))
                                .editing(self.edit_of(position))
                                .on_mouse_down(cx.listener(
                                    move |this, event: &MouseDownEvent, _, cx| {
                                        this.press_row(position, event, cx);
                                    },
                                ))
                                .on_context_menu(cx.listener(
                                    move |this, event: &MouseDownEvent, window, cx| {
                                        this.right_press_row(position, event, window, cx);
                                    },
                                ))
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<_>>();
                div()
                    .w_full()
                    .h(px(icon_grid::row_height(side)))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .gap(px(icon_grid::GAP))
                    .px(px(icon_grid::INSET))
                    .children(tiles)
                    .into_any_element()
            })
            .collect();
        let thumbnails = self.thumbnails.clone();
        cx.defer(move |cx| thumbnails.update(cx, |t, cx| t.want(wants, cx)));
        let first = icon_grid::positions(range.start, columns, len).start;
        let last = icon_grid::positions(range.end.saturating_sub(1), columns, len).end;
        let listing = self.listing.clone();
        cx.defer(move |cx| {
            listing.update(cx, |listing, cx| listing.note_visible(first..last, cx));
        });
        rows
    }

    /// The pointer came onto (`hovered`) or left the row at `position`.
    fn hover_row(&mut self, position: usize, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.hover.is_none_or(|(at, _)| at != position) {
                self.hover = Some((position, Instant::now()));
                cx.notify();
            }
        } else if self.hover.is_some_and(|(at, _)| at == position) {
            self.hover = None;
            cx.notify();
        }
    }

    /// Works out this frame's hover fill, selection plate and slide (`r1-redesign.md` 2.2) for
    /// `render_rows` and `render`, and asks for another frame while any of them moves.
    /// `content_shown` says whether anything of the current folder is drawn (rows or a state).
    /// Returns the slide's `(offset, opacity)` while it runs.
    fn motion_frame(
        &mut self,
        count: usize,
        content_shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(f32, f32)> {
        let now = Instant::now();
        let reduced = motion::reduced(cx);
        let row_height = Density::default().row_height();
        let (scroll_y, bounds) = {
            let state = self.scroll.0.borrow();
            (state.base_handle.offset().y, state.base_handle.bounds())
        };

        // Hover: only the row the pointer is over now. After a scroll the remembered row has
        // moved away from the pointer; it shows nothing until the pointer moves again.
        let under = row_under(window.mouse_position(), bounds, scroll_y, row_height, count);
        self.hover_paint = None;
        if let Some((position, since)) = self.hover.filter(|(at, _)| Some(*at) == under) {
            let amount = Tween::new(since, motion::QUICK).at(now);
            if amount.is_some() {
                window.request_animation_frame();
                crate::frame_trace::note(cx, "hover");
            }
            self.hover_paint = Some((position, amount.unwrap_or(1.0)));
        }

        // Selection plate.
        self.glide_paint = None;
        if let Some(glide) = self.glide {
            match glide.tween.at(now).filter(|_| !reduced) {
                Some(t) if self.listing.read(cx).cursor() == Some(glide.to) => {
                    let from = f32::from(row_height) * glide.from as f32;
                    let to = f32::from(row_height) * glide.to as f32;
                    self.glide_paint = Some((glide.to, px(motion::lerp(from, to, t))));
                    window.request_animation_frame();
                    crate::frame_trace::note(cx, "selection-glide");
                }
                _ => self.glide = None,
            }
        }

        // Folder navigation.
        if let Some(Slide::Waiting { offset }) = self.slide
            && content_shown
        {
            self.slide = Some(Slide::Running(Entrance::new(
                offset,
                motion::STANDARD,
                now,
                reduced,
            )));
        }
        match self.slide {
            Some(Slide::Running(entrance)) => match entrance.at(now) {
                Some(frame) => {
                    window.request_animation_frame();
                    crate::frame_trace::note(cx, "folder-slide");
                    Some(frame)
                }
                None => {
                    self.slide = None;
                    None
                }
            },
            Some(Slide::Waiting { .. }) | None => None,
        }
    }

    /// A click on a column title.
    fn sort_by(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        self.listing.update(cx, |listing, cx| {
            if listing.in_results() {
                let spec = next_sort(listing.sort(), column, true);
                listing.set_sort(spec, cx);
            } else {
                listing.toggle_sort(column, cx);
            }
        });
    }

    /// "Clear filters": drops the filters and returns focus to the list.
    fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.clear_filters(cx));
        window.focus(&self.focus);
    }

    /// Relaunches the app with administrator rights. The prompt blocks, so it runs on the
    /// background executor; the app quits once the new copy has started.
    fn restart_elevated(&mut self, cx: &mut Context<Self>) {
        if self.restarting {
            return;
        }
        self.restarting = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { elevation::restart_elevated() })
                .await;
            match result {
                Ok(Restart::Started) => {
                    cx.update(|cx| cx.quit()).ok();
                }
                Ok(Restart::Declined) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not restart with administrator rights");
                }
            }
            this.update(cx, |this, _| this.restarting = false).ok();
        })
        .detach();
    }

    fn press(&mut self, _: &controls::Press, _: &mut Window, cx: &mut Context<Self>) {
        self.restart_elevated(cx);
    }

    /// The designed state for a source that cannot be browsed, centred in the content area.
    /// `label` names the volume or disk.
    fn render_unavailable(
        &self,
        availability: &Availability,
        label: SharedString,
        colors: Colors,
        cx: &Context<Self>,
    ) -> Div {
        let state = div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .gap(space::S16)
            .items_center()
            .justify_center()
            .px(space::S16);
        match availability {
            // Browsable sources are never in this state.
            Availability::Browsable => state,
            Availability::Unsupported { format } => state.child(state_message(
                format!("Disk Atlas cannot read {format} yet").into(),
                Some(label),
                &colors,
            )),
            Availability::Failed(text) => state.child(
                div()
                    .text_color(colors.ink3)
                    .text_center()
                    .child(text.clone()),
            ),
            Availability::NeedsElevation => {
                let can_restart = elevation::can_restart_elevated();
                let detail: SharedString = if can_restart {
                    permission_detail().into()
                } else {
                    format!("{} {}", permission_detail(), elevation::manual_hint()).into()
                };
                let state = state.child(state_message(
                    format!("{label} needs permission").into(),
                    Some(detail),
                    &colors,
                ));
                if can_restart {
                    state.child(
                        div()
                            .id("restart-elevated")
                            .key_context(BUTTON)
                            .track_focus(&self.restart_focus)
                            .on_action(cx.listener(Self::press))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .h(size::CONTROL)
                            .px(space::S16)
                            .rounded(radius::FIELD)
                            .border_1()
                            .border_color(colors.line)
                            .text_color(colors.ink)
                            .cursor_pointer()
                            .hover(move |style| style.bg(colors.hover))
                            .active(move |style| style.bg(colors.field))
                            .focus(move |style| style.border_color(colors.ink))
                            .on_click(cx.listener(|this, _, _, cx| this.restart_elevated(cx)))
                            .child("Allow reading disks"),
                    )
                } else {
                    state
                }
            }
        }
    }

    /// A clickable column title; the active one is darker and shows the direction.
    fn header_label(
        &self,
        column: SortColumn,
        text: &'static str,
        sort: SortSpec,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Div {
        let mut label = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S4)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.sort_by(column, cx);
                }),
            );
        if sort.column != column {
            return label.child(text);
        }
        // The chevron points right; a quarter turn points it down, the other way up.
        let turn = if sort.descending {
            FRAC_PI_2
        } else {
            -FRAC_PI_2
        };
        let chevron = svg()
            .path(icon::CHEVRON)
            .flex_none()
            .w(metrics::CHEVRON_WIDTH)
            .h(metrics::CHEVRON_HEIGHT)
            .text_color(colors.ink)
            .with_transformation(Transformation::rotate(radians(turn)));
        label = label.text_color(colors.ink);
        // In the right-aligned Size and Resource fork columns the text stays flush right.
        if matches!(column, SortColumn::Size | SortColumn::ResourceFork) {
            label.child(chevron).child(text)
        } else {
            label.child(text).child(chevron)
        }
    }

    /// The state of a search that found nothing, centred in the content area. With filters on,
    /// it also offers to clear them.
    fn render_no_results(&self, no_results: &NoResults, colors: Colors, cx: &Context<Self>) -> Div {
        let lines = div()
            .flex()
            .flex_col()
            .items_center()
            .child(no_results.message.clone())
            .when(no_results.filters_on, |lines| {
                lines.child("Filters are on.")
            });
        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .gap(space::S16)
            .items_center()
            .justify_center()
            .px(space::S16)
            .text_color(colors.ink3)
            .child(lines)
            .when(no_results.filters_on, |state| {
                state.child(
                    div()
                        .id("clear-filters")
                        .key_context(BUTTON)
                        .track_focus(&self.clear_focus)
                        .on_action(cx.listener(|this, _: &controls::Press, window, cx| {
                            this.clear_filters(window, cx);
                        }))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .h(size::CONTROL)
                        .px(space::S16)
                        .rounded(radius::FIELD)
                        .border_1()
                        .border_color(colors.line)
                        .text_color(colors.ink)
                        .cursor_pointer()
                        .hover(move |style| style.bg(colors.hover))
                        .active(move |style| style.bg(colors.field))
                        .focus(move |style| style.border_color(colors.ink))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_filters(window, cx);
                        }))
                        .child("Clear filters"),
                )
            })
    }

    fn render_header(
        &self,
        sort: SortSpec,
        resource_column: bool,
        in_results: bool,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Div {
        let label = |column, text| self.header_label(column, text, sort, colors, cx);
        // Records the width the Folder column was given, for the rows to cut their paths to.
        let folder_width = self.folder_width.clone();
        let folder_probe = canvas(
            move |bounds, _, _| folder_width.set(bounds.size.width),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        row_frame()
            .text_size(text::SMALL.size)
            .text_color(colors.ink3)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.right_press_header(event.position, window, cx);
                }),
            )
            .child(
                columns(Pixels::ZERO)
                    .child(name_cell().child(label(SortColumn::Name, "Name")))
                    .when(in_results, |header| {
                        header.child(
                            folder_cell()
                                .relative()
                                .child(folder_probe)
                                .child(label(SortColumn::Folder, "Folder")),
                        )
                    })
                    .child(modified_cell().child(label(SortColumn::Modified, "Modified")))
                    .child(size_cell().child(label(SortColumn::Size, "Size")))
                    .when(resource_column, |header| {
                        header.child(
                            resource_cell().child(label(SortColumn::ResourceFork, "Resource fork")),
                        )
                    })
                    .child(kind_cell().child(label(SortColumn::Kind, "Kind"))),
            )
    }
}

impl Focusable for Explorer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let slide = {
            let listing = self.listing.read(cx);
            let count = listing.len();
            let content_shown = count > 0
                || listing.show_loading()
                || !matches!(listing.state(), LoadState::Loading | LoadState::Idle);
            self.motion_frame(count, content_shown, window, cx)
        };
        let plate = self.glide_paint.map(|(_, top)| {
            let scroll_y = self.scroll.0.borrow().base_handle.offset().y;
            // Under the rows: the row it is moving to leaves its own fill to it.
            div()
                .absolute()
                .top(top + scroll_y)
                .left(metrics::ROW_INSET)
                .right(metrics::ROW_INSET)
                .h(Density::default().row_height())
                .rounded(radius::ROW)
                .bg(colors.mark)
        });
        let listing = self.listing.read(cx);
        let count = listing.len();
        let sort = listing.sort();
        let resource_column = listing.resource_forks();
        let in_results = listing.in_results();
        let searching = self.search.read(cx).is_active();
        let no_results = (in_results && listing.is_empty()).then(|| {
            let search = self.search.read(cx);
            NoResults::new(
                search.text(),
                !listing.terms().is_empty(),
                search.filters().is_active(),
            )
        });
        let unavailable = match listing.state() {
            LoadState::Unavailable(availability) => Some(availability.clone()),
            _ => None,
        };
        let volume_label: SharedString = listing
            .location()
            .and_then(|location| location.crumbs().first())
            .map_or_else(|| "This disk".into(), |crumb| crumb.name.clone());
        let message: Option<(SharedString, Option<SharedString>)> = if no_results.is_some() {
            None
        } else if listing.show_loading() {
            Some(("Reading this folder\u{2026}".into(), None))
        } else {
            match listing.state() {
                LoadState::Ready if listing.is_empty() => {
                    let location = listing.location();
                    let folder = location.map(|location| location.name().clone());
                    let at_root = location.is_none_or(|location| location.crumbs().len() <= 1);
                    let detail = empty_folder_detail(
                        folder.as_ref().map_or("", |name| name.as_ref()),
                        &volume_label,
                        at_root,
                    );
                    Some(("This folder is empty".into(), Some(detail.into())))
                }
                LoadState::Failed(text) => Some((text.clone(), None)),
                LoadState::Idle
                | LoadState::Loading
                | LoadState::Ready
                | LoadState::Unavailable(_) => None,
            }
        };
        let note = self.ops_notice.clone().or_else(|| listing.note().cloned());

        let icons = self.icons(cx);
        let list = if icons {
            let width = f32::from(self.grid_width.get());
            self.grid_columns = icon_grid::columns_for(width, Self::tile_side());
            uniform_list(
                "icon-grid",
                icon_grid::row_count(count, self.grid_columns),
                cx.processor(Self::render_tiles),
            )
        } else {
            uniform_list("file-list", count, cx.processor(Self::render_rows))
        }
        .track_scroll(self.scroll.clone())
        .size_full()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.listing
                    .update(cx, |listing, cx| listing.clear_selection(cx));
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.right_press_empty(event.position, window, cx);
            }),
        );

        let list_area = div()
            .relative()
            .flex_1()
            .min_h_0()
            .when_some(slide, |area, (offset, opacity)| {
                area.left(px(offset)).opacity(opacity)
            })
            .children(plate)
            .child(list)
            .when(icons, |area| {
                // Measures the width the tiles flow in; a change draws the next frame again.
                let width = self.grid_width.clone();
                area.child(
                    canvas(
                        move |bounds, window, _| {
                            if width.replace(bounds.size.width) != bounds.size.width {
                                window.refresh();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .children(message.map(|(title, detail)| {
                let area = div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(space::S16)
                    .text_color(colors.ink3);
                if detail.is_none() {
                    area.child(title)
                } else {
                    area.child(state_message(title, detail, &colors))
                }
            }))
            .children(
                no_results
                    .as_ref()
                    .map(|no_results| self.render_no_results(no_results, colors, cx)),
            )
            .children(unavailable.as_ref().map(|availability| {
                self.render_unavailable(availability, volume_label.clone(), colors, cx)
            }));

        let list_view = div()
            .key_context(if self.renaming.is_some() {
                RENAMING
            } else {
                FILE_LIST
            })
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, _| {
                window.focus(&this.focus);
            }))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::move_to_first))
            .on_action(cx.listener(Self::move_to_last))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::select_to_first))
            .on_action(cx.listener(Self::select_to_last))
            .on_action(cx.listener(Self::open))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::move_to_trash))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::paste_items))
            .on_action(cx.listener(|this, _: &host_ops::CopyItems, _, cx| {
                this.remember_items(false, cx);
            }))
            .on_action(cx.listener(|this, _: &host_ops::CutItems, _, cx| {
                this.remember_items(true, cx);
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditLeft, _, cx| {
                this.edit(cx, |e| e.left(false));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditRight, _, cx| {
                this.edit(cx, |e| e.right(false));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditSelectLeft, _, cx| {
                this.edit(cx, |e| e.left(true));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditSelectRight, _, cx| {
                this.edit(cx, |e| e.right(true));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditHome, _, cx| {
                this.edit(cx, |e| e.home(false));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditEnd, _, cx| {
                this.edit(cx, |e| e.end(false));
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditBackspace, _, cx| {
                this.edit(cx, |e| e.backspace());
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditDelete, _, cx| {
                this.edit(cx, |e| e.delete());
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditSelectAll, _, cx| {
                this.edit(cx, |e| e.select_all());
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditCommit, window, cx| {
                this.commit_rename(window, cx);
            }))
            .on_action(cx.listener(|this, _: &host_ops::EditCancel, _, cx| {
                this.renaming = None;
                cx.notify();
            }))
            .on_action(cx.listener(Self::show_in_folder))
            .on_action(cx.listener(Self::show_package_contents))
            .on_action(cx.listener(Self::show_original))
            .on_action(cx.listener(Self::open_in_app))
            .on_action(cx.listener(Self::toggle_quick_look))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::open_menu))
            .on_action(cx.listener(Self::copy_folder_path))
            .on_action(cx.listener(Self::show_in_file_manager))
            .on_action(cx.listener(|this, _: &context::CopyName, _, cx| {
                this.copy_rows(CopyKind::Name, cx);
            }))
            .on_action(cx.listener(|this, _: &context::CopyPath, _, cx| {
                this.copy_rows(CopyKind::Path, cx);
            }))
            .on_action(cx.listener(|this, _: &context::CopyMacPath, _, cx| {
                this.copy_rows(CopyKind::MacPath, cx);
            }))
            .on_action(cx.listener(|this, _: &context::CopyNodeId, _, cx| {
                this.copy_rows(CopyKind::NodeId, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByName, _, cx| {
                this.sort_column(SortColumn::Name, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByModified, _, cx| {
                this.sort_column(SortColumn::Modified, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortBySize, _, cx| {
                this.sort_column(SortColumn::Size, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByResourceFork, _, cx| {
                this.sort_column(SortColumn::ResourceFork, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByKind, _, cx| {
                this.sort_column(SortColumn::Kind, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByFolder, _, cx| {
                this.sort_column(SortColumn::Folder, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortByRelevance, _, cx| {
                this.sort_column(SortColumn::Relevance, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortAscending, _, cx| {
                this.sort_direction(false, cx);
            }))
            .on_action(cx.listener(|this, _: &context::SortDescending, _, cx| {
                this.sort_direction(true, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.key_down(event, cx);
            }))
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            // A source that cannot be browsed has no columns to show.
            .when(unavailable.is_none() && !icons, |view| {
                view.child(self.render_header(sort, resource_column, in_results, &colors, cx))
            })
            .child(list_area)
            .children(self.menu.entity())
            .children(note.map(|text| {
                row_frame().child(
                    columns(Pixels::ZERO)
                        .text_size(text::SMALL.size)
                        .text_color(colors.ink3)
                        .child(div().truncate().child(text)),
                )
            }));

        // The filter row sits outside the list's key context and focus capture, so a click in
        // its menus does not move focus to the list and its keys never reach type-to-select.
        div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .when(searching, |view| view.child(self.filter_row.clone()))
            .child(list_view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer and a clock to step by hand.
    struct Typist {
        buffer: TypeBuffer,
        start: Instant,
    }

    impl Typist {
        fn new() -> Self {
            Self {
                buffer: TypeBuffer::default(),
                start: Instant::now(),
            }
        }

        fn at(&self, millis: u64) -> Instant {
            self.start + Duration::from_millis(millis)
        }
    }

    /// The names the fake list holds, matched by prefix.
    fn names(prefix: &str) -> bool {
        ["alpha", "beta", "bravo", "x y"]
            .iter()
            .any(|name| name.starts_with(prefix))
    }

    #[test]
    fn deeper_slides_from_the_right_and_up_from_the_left() {
        assert_eq!(slide_offset(2, 3), motion::FOLDER_SLIDE);
        assert_eq!(slide_offset(3, 3), motion::FOLDER_SLIDE);
        assert_eq!(slide_offset(3, 1), -motion::FOLDER_SLIDE);
    }

    #[test]
    fn the_row_under_the_pointer_follows_the_scroll() {
        let bounds = Bounds::new(point(px(100.), px(50.)), gpui::size(px(400.), px(240.)));
        let row = px(24.);
        let at = |x: f32, y: f32| point(px(x), px(y));
        // Unscrolled: the first row, the third row.
        assert_eq!(row_under(at(200., 50.), bounds, px(0.), row, 100), Some(0));
        assert_eq!(
            row_under(at(200., 50. + 24. * 2. + 5.), bounds, px(0.), row, 100),
            Some(2)
        );
        // Scrolled down by ten rows: the same pointer is over row 12.
        assert_eq!(
            row_under(at(200., 105.), bounds, px(-240.), row, 100),
            Some(12)
        );
        // Outside the list, past the last row, or a zero row height: nothing.
        assert_eq!(row_under(at(50., 60.), bounds, px(0.), row, 100), None);
        assert_eq!(row_under(at(200., 40.), bounds, px(0.), row, 100), None);
        assert_eq!(row_under(at(200., 280.), bounds, px(0.), row, 3), None);
        assert_eq!(row_under(at(200., 60.), bounds, px(0.), px(0.), 100), None);
    }

    #[test]
    fn a_row_is_fully_visible_only_inside_the_list() {
        let (row, height) = (px(24.), px(240.));
        assert!(row_fully_visible(0, px(0.), height, row));
        assert!(row_fully_visible(9, px(0.), height, row));
        assert!(!row_fully_visible(10, px(0.), height, row));
        assert!(!row_fully_visible(0, px(-12.), height, row));
        assert!(row_fully_visible(10, px(-24.), height, row));
    }

    #[test]
    fn grows_within_the_timeout() {
        let mut typist = Typist::new();
        assert!(typist.buffer.type_char('b', typist.at(0), names));
        assert!(typist.buffer.type_char('r', typist.at(900), names));
        assert_eq!(typist.buffer.text, "br");
    }

    #[test]
    fn exactly_the_timeout_still_grows() {
        let mut typist = Typist::new();
        typist.buffer.type_char('b', typist.at(0), names);
        typist.buffer.type_char('r', typist.at(1000), names);
        assert_eq!(typist.buffer.text, "br");
    }

    #[test]
    fn starts_over_after_the_timeout() {
        let mut typist = Typist::new();
        typist.buffer.type_char('b', typist.at(0), names);
        assert!(typist.buffer.type_char('a', typist.at(1001), names));
        assert_eq!(typist.buffer.text, "a");
    }

    #[test]
    fn restarts_with_the_last_character_when_nothing_matches() {
        let mut typist = Typist::new();
        typist.buffer.type_char('z', typist.at(0), |_| false);
        let mut seen = Vec::new();
        let matched = typist.buffer.type_char('b', typist.at(100), |prefix| {
            seen.push(prefix.to_owned());
            names(prefix)
        });
        assert!(matched);
        assert_eq!(seen, ["zb", "b"]);
        assert_eq!(typist.buffer.text, "b");
    }

    #[test]
    fn a_single_unmatched_character_is_tried_once() {
        let mut typist = Typist::new();
        let mut tries = 0;
        let matched = typist.buffer.type_char('q', typist.at(0), |_| {
            tries += 1;
            false
        });
        assert!(!matched);
        assert_eq!(tries, 1);
        assert_eq!(typist.buffer.text, "q");
    }

    #[test]
    fn space_with_no_buffer_is_ignored() {
        let mut typist = Typist::new();
        let mut asked = false;
        let matched = typist.buffer.type_char(' ', typist.at(0), |_| {
            asked = true;
            true
        });
        assert!(!matched);
        assert!(!asked);
        assert!(typist.buffer.text.is_empty());
    }

    #[test]
    fn space_after_the_timeout_is_ignored() {
        let mut typist = Typist::new();
        typist.buffer.type_char('x', typist.at(0), names);
        assert!(!typist.buffer.type_char(' ', typist.at(2000), names));
        assert!(!typist.buffer.is_active(typist.at(2000)));
    }

    #[test]
    fn space_inside_a_name_is_part_of_it() {
        let mut typist = Typist::new();
        typist.buffer.type_char('x', typist.at(0), names);
        assert!(typist.buffer.type_char(' ', typist.at(300), names));
        assert!(typist.buffer.type_char('y', typist.at(600), names));
        assert_eq!(typist.buffer.text, "x y");
    }

    fn spec(column: SortColumn, descending: bool) -> SortSpec {
        SortSpec { column, descending }
    }

    #[test]
    fn results_cycle_ascending_descending_relevance() {
        let relevance = spec(SortColumn::Relevance, false);
        let name_up = next_sort(relevance, SortColumn::Name, true);
        assert_eq!(name_up, spec(SortColumn::Name, false));
        let name_down = next_sort(name_up, SortColumn::Name, true);
        assert_eq!(name_down, spec(SortColumn::Name, true));
        assert_eq!(next_sort(name_down, SortColumn::Name, true), relevance);
    }

    #[test]
    fn results_start_another_column_ascending() {
        let current = spec(SortColumn::Name, true);
        assert_eq!(
            next_sort(current, SortColumn::Folder, true),
            spec(SortColumn::Folder, false)
        );
        assert_eq!(
            next_sort(spec(SortColumn::Relevance, false), SortColumn::Folder, true),
            spec(SortColumn::Folder, false)
        );
    }

    #[test]
    fn a_folder_flips_the_direction_and_never_leaves_the_column() {
        let name_up = spec(SortColumn::Name, false);
        let name_down = next_sort(name_up, SortColumn::Name, false);
        assert_eq!(name_down, spec(SortColumn::Name, true));
        assert_eq!(next_sort(name_down, SortColumn::Name, false), name_up);
        assert_eq!(
            next_sort(name_up, SortColumn::Size, false),
            spec(SortColumn::Size, false)
        );
    }

    #[test]
    fn nothing_found_for_a_query_names_the_query() {
        assert_eq!(
            NoResults::new("report", true, false),
            NoResults {
                message: "No names contain \u{201c}report\u{201d}.".to_owned(),
                filters_on: false,
            }
        );
        // The query is shown as typed, minus the spaces around it.
        assert_eq!(
            NoResults::new("  two words ", true, false).message,
            "No names contain \u{201c}two words\u{201d}."
        );
    }

    #[test]
    fn filters_alone_have_their_own_sentence() {
        assert_eq!(
            NoResults::new("", false, true),
            NoResults {
                message: "Nothing matches these filters.".to_owned(),
                filters_on: true,
            }
        );
        // Spaces are not terms.
        assert_eq!(
            NoResults::new("   ", false, true).message,
            "Nothing matches these filters."
        );
    }

    #[test]
    fn filters_on_adds_the_clear_offer_to_either_sentence() {
        assert!(NoResults::new("report", true, true).filters_on);
        assert!(NoResults::new("", false, true).filters_on);
        assert!(!NoResults::new("report", true, false).filters_on);
        assert!(!NoResults::new("", false, false).filters_on);
    }

    #[test]
    fn clear_ends_the_name() {
        let mut typist = Typist::new();
        typist.buffer.type_char('b', typist.at(0), names);
        typist.buffer.clear();
        assert!(!typist.buffer.is_active(typist.at(10)));
        assert!(!typist.buffer.type_char(' ', typist.at(20), names));
    }
}
