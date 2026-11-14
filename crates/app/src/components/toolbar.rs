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

//! The filter row: Where, Kind, Size, Date modified and Hidden items, shown under the top bar
//! while a search is active.
//!
//! Each control but the last opens a small menu under it. Every change goes to
//! [`Search::set_filters`]; the row keeps no filter of its own beyond which date preset was
//! chosen (a preset becomes a range at the moment it is chosen, so the range alone cannot name
//! it).

use std::cell::Cell;
use std::f32::consts::FRAC_PI_2;
use std::ops::RangeInclusive;
use std::rc::Rc;

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use disk_atlas_lib::search::kind::KindClass;
use gpui::{
    Action, Bounds, Context, Div, Entity, FocusHandle, IntoElement, Pixels, Render, Stateful,
    Subscription, Transformation, Window, canvas, div, point, prelude::*, radians, svg,
};

use crate::assets::icon;
use crate::components::menu::{Menu, MenuEvent, MenuItem};
use crate::keymap::{BUTTON, controls};
use crate::state::search::{Filters, Search};
use crate::theme::{
    Colors, Theme, metrics,
    scale::{radius, size, space, text},
};

/// Bytes in the "MB" of the Size menu. The same base as `format::size`, so a file the Size
/// column calls "1.0 MB" is over the line in "1-100 MB".
const MB: u64 = 1 << 20;

/// Bytes in the "GB" of the Size menu.
const GB: u64 = 1 << 30;

/// Nanoseconds in a day (24 hours).
const DAY_NANOS: i64 = 86_400 * 1_000_000_000;

/// One of the five controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Where,
    Kind,
    Size,
    Modified,
    Hidden,
}

impl Control {
    const ALL: [Control; 5] = [
        Control::Where,
        Control::Kind,
        Control::Size,
        Control::Modified,
        Control::Hidden,
    ];

    /// Position in [`Self::ALL`]: the control's focus handle and element id.
    fn index(self) -> usize {
        match self {
            Control::Where => 0,
            Control::Kind => 1,
            Control::Size => 2,
            Control::Modified => 3,
            Control::Hidden => 4,
        }
    }

    /// True for the controls that open a menu; Hidden items is a plain toggle.
    fn has_menu(self) -> bool {
        self != Control::Hidden
    }

    /// The name on the control.
    fn name(self) -> &'static str {
        match self {
            Control::Where => "Where",
            Control::Kind => "Kind",
            Control::Size => "Size",
            Control::Modified => "Date modified",
            Control::Hidden => "Hidden items",
        }
    }

    /// Rows in the control's menu.
    #[cfg(test)]
    fn item_count(self) -> usize {
        match self {
            Control::Where => WHERE_LABELS.len(),
            Control::Kind => KindClass::ALL.len(),
            Control::Size => SizeChoice::ALL.len(),
            Control::Modified => DateChoice::ALL.len(),
            Control::Hidden => 0,
        }
    }
}

/// The two scopes of the Where menu, in menu order: index 1 is `this_folder`.
const WHERE_LABELS: [&str; 2] = ["This volume", "This folder"];

/// A row of the Size menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeChoice {
    Any,
    Under1Mb,
    From1To100Mb,
    From100MbTo1Gb,
    Over1Gb,
}

impl SizeChoice {
    const ALL: [SizeChoice; 5] = [
        SizeChoice::Any,
        SizeChoice::Under1Mb,
        SizeChoice::From1To100Mb,
        SizeChoice::From100MbTo1Gb,
        SizeChoice::Over1Gb,
    ];

    fn label(self) -> &'static str {
        match self {
            SizeChoice::Any => "Any",
            SizeChoice::Under1Mb => "Under 1 MB",
            SizeChoice::From1To100Mb => "1\u{2013}100 MB",
            SizeChoice::From100MbTo1Gb => "100 MB\u{2013}1 GB",
            SizeChoice::Over1Gb => "Over 1 GB",
        }
    }

    /// The sizes the choice admits, in bytes. Neighbours share an edge without overlapping:
    /// each range ends one byte before the next begins.
    fn range(self) -> Option<RangeInclusive<u64>> {
        match self {
            SizeChoice::Any => None,
            SizeChoice::Under1Mb => Some(0..=MB - 1),
            SizeChoice::From1To100Mb => Some(MB..=100 * MB - 1),
            SizeChoice::From100MbTo1Gb => Some(100 * MB..=GB - 1),
            SizeChoice::Over1Gb => Some(GB..=u64::MAX),
        }
    }
}

/// A row of the Date modified menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateChoice {
    Any,
    Today,
    Last7Days,
    Last30Days,
    ThisYear,
    BeforeThisYear,
}

impl DateChoice {
    const ALL: [DateChoice; 6] = [
        DateChoice::Any,
        DateChoice::Today,
        DateChoice::Last7Days,
        DateChoice::Last30Days,
        DateChoice::ThisYear,
        DateChoice::BeforeThisYear,
    ];

    fn label(self) -> &'static str {
        match self {
            DateChoice::Any => "Any",
            DateChoice::Today => "Today",
            DateChoice::Last7Days => "Last 7 days",
            DateChoice::Last30Days => "Last 30 days",
            DateChoice::ThisYear => "This year",
            DateChoice::BeforeThisYear => "Before this year",
        }
    }

    /// The modification times the choice admits, in Unix nanoseconds, as of `now` in `now`'s own
    /// time zone. `None` for Any, and for a time that cannot be expressed in nanoseconds (the
    /// filter is then left off rather than guessed).
    fn range<Tz: TimeZone>(self, now: &DateTime<Tz>) -> Option<RangeInclusive<i64>> {
        let now_nanos = now.timestamp_nanos_opt()?;
        match self {
            DateChoice::Any => None,
            DateChoice::Today => {
                Some(local_midnight(now.date_naive(), &now.timezone())?..=i64::MAX)
            }
            DateChoice::Last7Days => Some(now_nanos.checked_sub(7 * DAY_NANOS)?..=i64::MAX),
            DateChoice::Last30Days => Some(now_nanos.checked_sub(30 * DAY_NANOS)?..=i64::MAX),
            DateChoice::ThisYear => Some(new_year(now)?..=i64::MAX),
            DateChoice::BeforeThisYear => Some(i64::MIN + 1..=new_year(now)?.checked_sub(1)?),
        }
    }
}

/// The first instant of `date` in `tz`, in Unix nanoseconds. When a daylight saving jump skips
/// midnight, the first instant that exists after it.
fn local_midnight<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Option<i64> {
    (0..4).find_map(|hour| {
        let naive = date.and_hms_opt(hour, 0, 0)?;
        tz.from_local_datetime(&naive)
            .earliest()?
            .timestamp_nanos_opt()
    })
}

/// 1 January 00:00 of `now`'s year in its time zone, in Unix nanoseconds.
fn new_year<Tz: TimeZone>(now: &DateTime<Tz>) -> Option<i64> {
    local_midnight(NaiveDate::from_ymd_opt(now.year(), 1, 1)?, &now.timezone())
}

/// The filters after choosing row `index` of `control`'s menu (or flipping Hidden items), or
/// `None` when `index` is not a row. `now` turns a date preset into a range.
fn choose<Tz: TimeZone>(
    control: Control,
    index: usize,
    filters: &Filters,
    now: &DateTime<Tz>,
) -> Option<Filters> {
    let mut next = filters.clone();
    match control {
        Control::Where => {
            if index >= WHERE_LABELS.len() {
                return None;
            }
            next.this_folder = index == 1;
        }
        Control::Kind => {
            let class = *KindClass::ALL.get(index)?;
            next.kinds = if next.kinds.contains(class) {
                next.kinds.without(class)
            } else {
                next.kinds.with(class)
            };
        }
        Control::Size => next.size = SizeChoice::ALL.get(index)?.range(),
        Control::Modified => next.modified = DateChoice::ALL.get(index)?.range(now),
        Control::Hidden => next.include_hidden = !next.include_hidden,
    }
    Some(next)
}

/// The date row to show as chosen: the one remembered while a date filter is set, Any otherwise
/// (the filters may have been cleared from elsewhere).
fn modified_index(filters: &Filters, remembered: usize) -> usize {
    if filters.modified.is_some() {
        remembered
    } else {
        0
    }
}

/// True when `control` narrows the search (or, for Hidden items, widens it) and is drawn filled.
fn is_active(control: Control, filters: &Filters) -> bool {
    match control {
        Control::Where => filters.this_folder,
        Control::Kind => !filters.kinds.is_empty(),
        Control::Size => filters.size.is_some(),
        Control::Modified => filters.modified.is_some(),
        Control::Hidden => filters.include_hidden,
    }
}

/// The classes in `filters`, as the control shows them: up to two names, otherwise a count.
fn kind_value(filters: &Filters) -> String {
    let names: Vec<&str> = KindClass::ALL
        .iter()
        .filter(|class| filters.kinds.contains(**class))
        .map(|class| class.label())
        .collect();
    if names.len() <= 2 {
        names.join(", ")
    } else {
        format!("{} kinds", names.len())
    }
}

/// The text on `control`: its name, then ": value" when it has one worth showing. Where always
/// names its scope; Hidden items is just its name.
fn control_text(control: Control, filters: &Filters, remembered_date: usize) -> String {
    let name = control.name();
    match control {
        Control::Where => {
            let scope = WHERE_LABELS[usize::from(filters.this_folder)];
            format!("{name}: {scope}")
        }
        Control::Kind if !filters.kinds.is_empty() => format!("{name}: {}", kind_value(filters)),
        Control::Size if filters.size.is_some() => {
            let label = SizeChoice::ALL
                .iter()
                .find(|choice| choice.range() == filters.size)
                .map_or("Custom", |choice| choice.label());
            format!("{name}: {label}")
        }
        Control::Modified if filters.modified.is_some() => {
            let index = modified_index(filters, remembered_date);
            let label = DateChoice::ALL
                .get(index)
                .map_or("Custom", |choice| choice.label());
            format!("{name}: {label}")
        }
        _ => name.to_owned(),
    }
}

/// The rows of `control`'s menu: a label and whether it is checked.
fn menu_items(
    control: Control,
    filters: &Filters,
    remembered_date: usize,
) -> Vec<(&'static str, bool)> {
    match control {
        Control::Where => WHERE_LABELS
            .iter()
            .enumerate()
            .map(|(index, label)| (*label, (index == 1) == filters.this_folder))
            .collect(),
        Control::Kind => KindClass::ALL
            .iter()
            .map(|class| (class.label(), filters.kinds.contains(*class)))
            .collect(),
        Control::Size => SizeChoice::ALL
            .iter()
            .map(|choice| (choice.label(), choice.range() == filters.size))
            .collect(),
        Control::Modified => {
            let current = modified_index(filters, remembered_date);
            DateChoice::ALL
                .iter()
                .enumerate()
                .map(|(index, choice)| (choice.label(), index == current))
                .collect()
        }
        Control::Hidden => Vec::new(),
    }
}

/// The row the cursor starts on when `control`'s menu opens: the first checked one.
fn first_checked(control: Control, filters: &Filters, remembered_date: usize) -> usize {
    menu_items(control, filters, remembered_date)
        .iter()
        .position(|(_, checked)| *checked)
        .unwrap_or(0)
}

/// Row `index` of a filter menu was chosen. Dispatched to the control that opened the menu.
#[derive(Clone, PartialEq, Action)]
#[action(namespace = filter, no_json)]
struct Pick {
    index: usize,
}

/// The rows of `control`'s menu as menu items. The Kind menu stays open, so each of its rows can
/// be toggled in turn.
fn menu_entries(control: Control, filters: &Filters, remembered_date: usize) -> Vec<MenuItem> {
    menu_items(control, filters, remembered_date)
        .into_iter()
        .enumerate()
        .map(|(index, (label, checked))| {
            let item = MenuItem::action(label, Pick { index }).checked(checked);
            if control == Control::Kind {
                item.keep_open()
            } else {
                item
            }
        })
        .collect()
}

/// The filter row.
pub struct FilterRow {
    search: Entity<Search>,
    /// One tab stop per control, in [`Control::ALL`] order.
    controls: [FocusHandle; 5],
    /// Where each control was last drawn, for placing its menu under it.
    bounds: [Rc<Cell<Bounds<Pixels>>>; 5],
    open: Option<Control>,
    /// The open menu.
    menu: Option<Entity<Menu>>,
    menu_subscription: Option<Subscription>,
    /// The Date modified row last chosen.
    modified_choice: usize,
    _subscriptions: Vec<Subscription>,
}

impl FilterRow {
    /// The row over `search`.
    pub fn new(search: Entity<Search>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![cx.subscribe(&search, |this, search, _, cx| {
            // A search that ended takes its menu with it.
            if !search.read(cx).is_active() {
                this.open = None;
                this.menu = None;
            }
            cx.notify();
        })];
        Self {
            search,
            controls: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            bounds: std::array::from_fn(|_| Rc::new(Cell::new(Bounds::default()))),
            open: None,
            menu: None,
            menu_subscription: None,
            modified_choice: 0,
            _subscriptions: subscriptions,
        }
    }

    /// The control was pressed: open its menu, or flip the toggle.
    fn activate(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        if !control.has_menu() {
            self.apply(control, 0, cx);
            return;
        }
        if self.open == Some(control) {
            return;
        }
        let filters = self.search.read(cx).filters().clone();
        let items = menu_entries(control, &filters, self.modified_choice);
        let cursor = first_checked(control, &filters, self.modified_choice);
        let bounds = self.bounds[control.index()].get();
        let position = point(bounds.left(), bounds.bottom() + space::S4);
        let target = self.controls[control.index()].clone();
        let menu = cx.new(|cx| {
            Menu::new(items, position, Some(target), window, cx)
                .wrap(false)
                .cursor(Some(cursor))
        });
        self.menu_subscription =
            Some(
                cx.subscribe(&menu, |this, _, event: &MenuEvent, cx| match event {
                    MenuEvent::Closed(_) => {
                        this.open = None;
                        this.menu = None;
                        cx.notify();
                    }
                }),
            );
        self.menu = Some(menu);
        self.open = Some(control);
        cx.notify();
    }

    /// Row `index` of `control`'s menu was chosen: sends the new filters to the search.
    fn apply(&mut self, control: Control, index: usize, cx: &mut Context<Self>) {
        let filters = self.search.read(cx).filters().clone();
        let Some(next) = choose(control, index, &filters, &Local::now()) else {
            return;
        };
        if control == Control::Modified {
            self.modified_choice = index;
        }
        self.search
            .update(cx, |search, cx| search.set_filters(next, cx));
        cx.notify();
    }

    /// The menu chose row `index`. A menu that stays open (Kind) shows the new checks.
    fn pick(
        &mut self,
        control: Control,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply(control, index, cx);
        if let Some(menu) = self.menu.clone()
            && self.open == Some(control)
        {
            let filters = self.search.read(cx).filters().clone();
            let items = menu_entries(control, &filters, self.modified_choice);
            menu.update(cx, |menu, cx| menu.set_items(items, window, cx));
        }
    }

    /// One control, with its menu when open.
    fn render_control(
        &self,
        control: Control,
        filters: &Filters,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Div {
        let active = is_active(control, filters);
        // A filled control keeps its edge in its own color; a focused one lightens it so the
        // focus is visible on the fill.
        let (fill, text, edge, edge_focused) = if active {
            (colors.ink, colors.sheet, colors.ink, colors.ink3)
        } else {
            (colors.field, colors.ink, colors.field, colors.ink)
        };
        let label = control_text(control, filters, self.modified_choice);
        let cell = Rc::clone(&self.bounds[control.index()]);

        let mut button: Stateful<Div> = div()
            .id(("filter-control", control.index()))
            .key_context(BUTTON)
            .track_focus(&self.controls[control.index()])
            .on_action(cx.listener(move |this, _: &controls::Press, window, cx| {
                this.activate(control, window, cx);
            }))
            .on_action(cx.listener(move |this, pick: &Pick, window, cx| {
                this.pick(control, pick.index, window, cx);
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate(control, window, cx);
            }))
            .relative()
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(space::S8)
            .h(size::CONTROL_SEGMENT)
            .px(metrics::FILTER_CONTROL_PADDING)
            .rounded(radius::ROW)
            .border_1()
            .border_color(edge)
            .bg(fill)
            .text_color(text)
            .text_size(text::SMALL.size)
            .whitespace_nowrap()
            .cursor_pointer()
            .when(!active, |button| {
                button
                    .hover(|style| style.bg(colors.hover))
                    .active(|style| style.bg(colors.line))
            })
            .focus(move |style| style.border_color(edge_focused))
            .child(label)
            .child(
                canvas(move |bounds, _, _| cell.set(bounds), |_, (), _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            );
        if control.has_menu() {
            // The chevron points right; a quarter turn points it down.
            button = button.child(
                svg()
                    .path(icon::CHEVRON)
                    .flex_none()
                    .w(metrics::CHEVRON_WIDTH)
                    .h(metrics::CHEVRON_HEIGHT)
                    .text_color(text)
                    .with_transformation(Transformation::rotate(radians(FRAC_PI_2))),
            );
        }

        let mut wrapper = div().relative().flex_none().child(button);
        if self.open == Some(control)
            && let Some(menu) = &self.menu
        {
            wrapper = wrapper.child(menu.clone());
        }
        wrapper
    }
}

impl Render for FilterRow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let search = self.search.read(cx);
        let row = div().flex_none().w_full();
        if !search.is_active() {
            return row;
        }
        let filters = search.filters().clone();
        row.flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(space::S8)
            .px(metrics::ROW_INSET + metrics::ROW_PADDING)
            .py(space::S8)
            .children(
                Control::ALL
                    .into_iter()
                    .map(|control| self.render_control(control, &filters, colors, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Utc};

    use super::*;
    use crate::format;

    fn filters() -> Filters {
        Filters::default()
    }

    /// 2026-11-12 `hour:minute` in a zone two hours east of UTC.
    fn local(hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(2 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 12, hour, minute, 0)
            .unwrap()
    }

    fn nanos<Tz: TimeZone>(time: DateTime<Tz>) -> i64 {
        time.timestamp_nanos_opt().unwrap()
    }

    #[test]
    fn size_edges_are_adjacent_and_do_not_overlap() {
        let ranges: Vec<_> = SizeChoice::ALL[1..]
            .iter()
            .map(|choice| choice.range().unwrap())
            .collect();
        assert_eq!(*ranges[0].start(), 0);
        assert_eq!(*ranges[3].end(), u64::MAX);
        for pair in ranges.windows(2) {
            assert_eq!(*pair[0].end() + 1, *pair[1].start());
        }
        assert_eq!(SizeChoice::Any.range(), None);
    }

    #[test]
    fn size_edges_use_the_unit_base_of_the_size_column() {
        // The Size column calls these bytes exactly 1 MB, 100 MB and 1 GB, so the menu's lines
        // sit on them: 1 MB belongs to "1-100 MB", not to "Under 1 MB".
        assert_eq!(format::size(MB), "1.0 MB");
        assert_eq!(format::size(100 * MB), "100.0 MB");
        assert_eq!(format::size(GB), "1.0 GB");
        let under = SizeChoice::Under1Mb.range().unwrap();
        assert!(under.contains(&(MB - 1)) && !under.contains(&MB));
        let middle = SizeChoice::From1To100Mb.range().unwrap();
        assert!(middle.contains(&MB) && middle.contains(&(100 * MB - 1)));
        assert!(!middle.contains(&(100 * MB)));
        let upper = SizeChoice::From100MbTo1Gb.range().unwrap();
        assert!(upper.contains(&(100 * MB)) && !upper.contains(&GB));
        assert!(SizeChoice::Over1Gb.range().unwrap().contains(&GB));
    }

    #[test]
    fn today_starts_at_local_midnight() {
        let now = local(14, 30);
        let midnight = nanos(local(0, 0));
        assert_eq!(DateChoice::Today.range(&now), Some(midnight..=i64::MAX));
    }

    #[test]
    fn just_after_midnight_is_still_today() {
        let now = local(0, 1);
        let midnight = nanos(local(0, 0));
        assert_eq!(DateChoice::Today.range(&now), Some(midnight..=i64::MAX));
        // One minute earlier it was the day before: UTC midnight is 02:00 local, so the same
        // instant in UTC is still yesterday.
        let utc = now.with_timezone(&Utc);
        assert_eq!(
            DateChoice::Today.range(&utc),
            Some(nanos(Utc.with_ymd_and_hms(2026, 11, 11, 0, 0, 0).unwrap())..=i64::MAX)
        );
    }

    #[test]
    fn last_days_count_whole_days_back_from_now() {
        let now = local(14, 30);
        assert_eq!(
            DateChoice::Last7Days.range(&now),
            Some(nanos(now) - 7 * DAY_NANOS..=i64::MAX)
        );
        assert_eq!(
            DateChoice::Last30Days.range(&now),
            Some(nanos(now) - 30 * DAY_NANOS..=i64::MAX)
        );
    }

    #[test]
    fn this_year_and_before_meet_without_a_gap() {
        let now = local(14, 30);
        let new_year = nanos(
            FixedOffset::east_opt(2 * 3600)
                .unwrap()
                .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap(),
        );
        assert_eq!(DateChoice::ThisYear.range(&now), Some(new_year..=i64::MAX));
        assert_eq!(
            DateChoice::BeforeThisYear.range(&now),
            Some(i64::MIN + 1..=new_year - 1)
        );
    }

    #[test]
    fn on_new_year_itself_today_and_this_year_agree() {
        let now = FixedOffset::east_opt(2 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap();
        let start = nanos(now);
        assert_eq!(DateChoice::Today.range(&now), Some(start..=i64::MAX));
        assert_eq!(DateChoice::ThisYear.range(&now), Some(start..=i64::MAX));
        assert_eq!(
            DateChoice::BeforeThisYear.range(&now),
            Some(i64::MIN + 1..=start - 1)
        );
        // The last second of the old year is "before this year" and not "this year".
        let last = start - 1_000_000_000;
        assert!(
            DateChoice::BeforeThisYear
                .range(&now)
                .unwrap()
                .contains(&last)
        );
        assert!(!DateChoice::ThisYear.range(&now).unwrap().contains(&last));
    }

    #[test]
    fn any_date_is_no_filter() {
        assert_eq!(DateChoice::Any.range(&local(9, 0)), None);
    }

    #[test]
    fn where_chooses_the_scope() {
        let now = local(9, 0);
        let folder = choose(Control::Where, 1, &filters(), &now).unwrap();
        assert!(folder.this_folder);
        let volume = choose(Control::Where, 0, &folder, &now).unwrap();
        assert!(!volume.this_folder);
        assert_eq!(choose(Control::Where, 2, &filters(), &now), None);
    }

    #[test]
    fn kind_rows_toggle_and_keep_the_others() {
        let now = local(9, 0);
        let image = KindClass::ALL
            .iter()
            .position(|class| *class == KindClass::Image)
            .unwrap();
        let code = KindClass::ALL
            .iter()
            .position(|class| *class == KindClass::Code)
            .unwrap();
        let one = choose(Control::Kind, image, &filters(), &now).unwrap();
        let two = choose(Control::Kind, code, &one, &now).unwrap();
        assert!(two.kinds.contains(KindClass::Image) && two.kinds.contains(KindClass::Code));
        let back = choose(Control::Kind, image, &two, &now).unwrap();
        assert!(!back.kinds.contains(KindClass::Image) && back.kinds.contains(KindClass::Code));
        assert_eq!(
            choose(Control::Kind, KindClass::ALL.len(), &one, &now),
            None
        );
    }

    #[test]
    fn size_and_date_rows_replace_the_range() {
        let now = local(9, 0);
        let sized = choose(Control::Size, 2, &filters(), &now).unwrap();
        assert_eq!(sized.size, SizeChoice::From1To100Mb.range());
        let cleared = choose(Control::Size, 0, &sized, &now).unwrap();
        assert_eq!(cleared.size, None);
        let dated = choose(Control::Modified, 1, &filters(), &now).unwrap();
        assert_eq!(dated.modified, DateChoice::Today.range(&now));
        assert_eq!(choose(Control::Modified, 6, &filters(), &now), None);
    }

    #[test]
    fn hidden_items_flips_and_is_not_a_filter() {
        let now = local(9, 0);
        let shown = choose(Control::Hidden, 0, &filters(), &now).unwrap();
        assert!(shown.include_hidden);
        assert!(is_active(Control::Hidden, &shown));
        assert!(!shown.is_active());
        assert!(
            !choose(Control::Hidden, 0, &shown, &now)
                .unwrap()
                .include_hidden
        );
    }

    #[test]
    fn controls_read_their_value_when_active() {
        let now = local(9, 0);
        let mut f = filters();
        assert_eq!(control_text(Control::Where, &f, 0), "Where: This volume");
        assert_eq!(control_text(Control::Kind, &f, 0), "Kind");
        assert_eq!(control_text(Control::Size, &f, 0), "Size");
        assert_eq!(control_text(Control::Modified, &f, 0), "Date modified");
        assert_eq!(control_text(Control::Hidden, &f, 0), "Hidden items");
        for control in [
            Control::Where,
            Control::Kind,
            Control::Size,
            Control::Modified,
        ] {
            assert!(!is_active(control, &f));
        }

        f = choose(Control::Where, 1, &f, &now).unwrap();
        f = choose(Control::Size, 2, &f, &now).unwrap();
        f = choose(Control::Modified, 3, &f, &now).unwrap();
        f = choose(Control::Kind, 3, &f, &now).unwrap();
        assert_eq!(control_text(Control::Where, &f, 3), "Where: This folder");
        assert_eq!(control_text(Control::Size, &f, 3), "Size: 1\u{2013}100 MB");
        assert_eq!(
            control_text(Control::Modified, &f, 3),
            "Date modified: Last 30 days"
        );
        assert_eq!(control_text(Control::Kind, &f, 3), "Kind: Picture");
        for control in [
            Control::Where,
            Control::Kind,
            Control::Size,
            Control::Modified,
        ] {
            assert!(is_active(control, &f));
        }
        // Cleared from elsewhere: the remembered row is ignored.
        f.modified = None;
        assert_eq!(control_text(Control::Modified, &f, 3), "Date modified");
    }

    #[test]
    fn kind_value_lists_two_and_counts_more() {
        let now = local(9, 0);
        let mut f = filters();
        for index in 0..3 {
            f = choose(Control::Kind, index, &f, &now).unwrap();
            let expected = match index {
                0 => "Other".to_owned(),
                1 => "Other, Folder".to_owned(),
                _ => "3 kinds".to_owned(),
            };
            assert_eq!(kind_value(&f), expected);
        }
    }

    #[test]
    fn menus_check_the_current_choice() {
        let now = local(9, 0);
        let f = choose(Control::Size, 3, &filters(), &now).unwrap();
        let items = menu_items(Control::Size, &f, 0);
        assert_eq!(items.len(), Control::Size.item_count());
        assert_eq!(first_checked(Control::Size, &f, 0), 3);
        assert_eq!(items.iter().filter(|(_, checked)| *checked).count(), 1);
        // Any is checked when nothing is set.
        assert_eq!(first_checked(Control::Size, &filters(), 0), 0);
        assert_eq!(first_checked(Control::Modified, &filters(), 4), 0);
        assert_eq!(
            menu_items(Control::Where, &filters(), 0)[0],
            ("This volume", true)
        );
        assert_eq!(Control::Kind.item_count(), KindClass::ALL.len());
        assert_eq!(Control::Modified.item_count(), 6);
    }
}
