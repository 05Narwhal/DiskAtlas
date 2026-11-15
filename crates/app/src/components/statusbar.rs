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

//! The footer line under the file list: how many items, how many are selected, and where the
//! search index of the volume stands. While a copy to this computer runs it shows the copy's
//! progress instead, and after one it shows how it went until the next navigation or copy
//! (`m5-quicklook-copy.md` 6). The strip that asks about resource forks sits above it, and the
//! copy options menu at its right end.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, Bounds, Context, Div, Entity, FocusHandle, IntoElement, Pixels, Render,
    SharedString, Stateful, Subscription, Window, div, point, prelude::*, relative, svg,
};

use crate::assets::icon;
use crate::components::copy_ui::{self, DetailsPanel, ForkStrip, outline_button};
use crate::components::menu::{MenuItem, MenuSlot};
use crate::components::sidebar::tooltip;
use crate::format;
use crate::keymap::{BUTTON, controls, transfers as actions};
use crate::state::listing::Listing;
use crate::state::search::{IndexState, Search, SearchEvent};
use crate::state::transfers::{Notice, NoticeKind, Transfers};
use crate::state::visibility::HIDDEN_TIP;
use crate::theme::{
    Colors, Theme, metrics,
    scale::{radius, size, space, text},
};

/// A no-break space: gives an empty footer the height of a line of text.
const BLANK_LINE: &str = "\u{a0}";

/// The tooltip of the footer's "Cancel".
const CANCEL_TIP: &str = "Stop copying. What was already copied stays where it is.";

/// The tooltip of the copy options button.
const OPTIONS_TIP: &str = "What a copy to this computer includes";

/// The footer line under the file list.
pub struct StatusBar {
    listing: Entity<Listing>,
    search: Entity<Search>,
    transfers: Entity<Transfers>,
    /// The fork question above the footer.
    strip: Entity<ForkStrip>,
    /// The panel behind "Details".
    panel: Entity<DetailsPanel>,
    /// The "hidden" button.
    focus: FocusHandle,
    /// The footer's "Cancel", "Show", "Details" and "Dismiss", and the options button.
    cancel_focus: FocusHandle,
    show_focus: FocusHandle,
    details_focus: FocusHandle,
    dismiss_focus: FocusHandle,
    options_focus: FocusHandle,
    /// Where the "Details" button and the options button were last drawn.
    details_at: Rc<Cell<Bounds<Pixels>>>,
    options_at: Rc<Cell<Bounds<Pixels>>>,
    /// The copy options menu, when open.
    menu: MenuSlot,
    _subscriptions: Vec<Subscription>,
}

impl StatusBar {
    /// Reports on the rows and the selection of `listing`, on the index state `search` holds and
    /// on the copies `transfers` runs.
    pub fn new(
        listing: Entity<Listing>,
        search: Entity<Search>,
        transfers: Entity<Transfers>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&listing, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, _: &SearchEvent, cx| cx.notify()),
            cx.observe(&transfers, |_, _, cx| cx.notify()),
        ];
        let details_at = Rc::new(Cell::new(Bounds::default()));
        let strip = cx.new(|cx| ForkStrip::new(transfers.clone(), window, cx));
        let panel =
            cx.new(|cx| DetailsPanel::new(transfers.clone(), Rc::clone(&details_at), window, cx));
        Self {
            listing,
            search,
            transfers,
            strip,
            panel,
            focus: cx.focus_handle().tab_stop(true),
            cancel_focus: cx.focus_handle().tab_stop(true),
            show_focus: cx.focus_handle().tab_stop(true),
            details_focus: cx.focus_handle().tab_stop(true),
            dismiss_focus: cx.focus_handle().tab_stop(true),
            options_focus: cx.focus_handle().tab_stop(true),
            details_at,
            options_at: Rc::new(Cell::new(Bounds::default())),
            menu: MenuSlot::default(),
            _subscriptions: subscriptions,
        }
    }
}

/// The rows of the copy options menu: the fork switch (greyed on a volume without forks) and
/// "Also copy housekeeping files". Both stay open when chosen, so each can be turned over in turn.
pub fn copy_menu_items(forks_on: bool, system_on: bool, volume_has_forks: bool) -> Vec<MenuItem> {
    let forks = MenuItem::action(
        "Keep resource forks and Finder information",
        actions::ToggleForks,
    )
    .checked(forks_on);
    let forks = if volume_has_forks {
        forks.keep_open()
    } else {
        forks.disabled("This volume has no resource forks.")
    };
    vec![
        forks,
        MenuItem::action("Also copy housekeeping files", actions::ToggleSystemItems)
            .checked(system_on)
            .keep_open(),
    ]
}

/// A button that is only words: the ink edge shows its focus, the wash its hover.
fn text_button(id: &'static str, focus: &FocusHandle, colors: &Colors) -> Stateful<Div> {
    let ink = colors.ink;
    div()
        .id(id)
        .key_context(BUTTON)
        .track_focus(focus)
        .flex_none()
        .px(space::S4)
        .rounded(radius::ROW)
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .text_color(colors.ink)
        .hover(|style| style.bg(colors.hover))
        .focus(move |style| style.border_color(ink))
}

impl StatusBar {
    /// "Cancel", "Show", "Details" and the close mark, and the copy options button: the controls
    /// of the copy that are not part of the item count.
    fn cancel_button(&self, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let tip = SharedString::new_static(CANCEL_TIP);
        outline_button("status-copy-cancel", "Cancel", &self.cancel_focus, colors)
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.cancel_copy(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.cancel_copy(cx)))
            .tooltip(move |_, cx| tooltip(tip.clone(), cx))
            .into_any_element()
    }

    fn cancel_copy(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.cancel(cx));
    }

    /// The notice of the last copy, with its buttons.
    fn notice_row(
        &self,
        notice: &Notice,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = vec![
            div()
                .min_w_0()
                .truncate()
                .text_color(colors.ink2)
                .child(SharedString::from(notice.text.clone()))
                .into_any_element(),
        ];
        if let Some(path) = notice.reveal.clone()
            && notice.kind != NoticeKind::Failed
        {
            out.push(
                text_button("status-copy-show", &self.show_focus, colors)
                    .on_action({
                        let path = path.clone();
                        cx.listener(move |_, _: &controls::Press, _, cx| cx.reveal_path(&path))
                    })
                    .on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&path)))
                    .child("Show")
                    .into_any_element(),
            );
        }
        if notice.details.is_some() {
            out.push(
                text_button("status-copy-details", &self.details_focus, colors)
                    .relative()
                    .on_action(cx.listener(|this, _: &controls::Press, _, cx| {
                        this.open_details(cx);
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.open_details(cx)))
                    .child("Details")
                    .child(copy_ui::measure(Rc::clone(&self.details_at)))
                    .into_any_element(),
            );
        }
        let ink3 = colors.ink3;
        out.push(div().flex_1().into_any_element());
        out.push(
            text_button("status-copy-dismiss", &self.dismiss_focus, colors)
                .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.dismiss(cx)))
                .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx)))
                .tooltip(|_, cx| tooltip(SharedString::new_static("Dismiss"), cx))
                .flex()
                .items_center()
                .child(
                    svg()
                        .path(icon::CLOSE)
                        .size(metrics::ICON_CAPTION)
                        .text_color(ink3),
                )
                .into_any_element(),
        );
        out
    }

    fn open_details(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.set_details_open(true, cx));
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.dismiss_notice(cx));
    }

    /// The copy options button, with its menu when open.
    fn options_button(&self, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let tip = SharedString::new_static(OPTIONS_TIP);
        let button = text_button("status-copy-options", &self.options_focus, colors)
            .relative()
            .flex()
            .items_center()
            .gap(space::S4)
            .text_color(colors.ink2)
            .on_action(cx.listener(|this, _: &controls::Press, window, cx| {
                this.open_copy_menu(window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleForks, window, cx| {
                this.transfers
                    .update(cx, |transfers, cx| transfers.toggle_forks(cx));
                this.refresh_menu(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleSystemItems, window, cx| {
                    this.transfers
                        .update(cx, |transfers, cx| transfers.toggle_system_items(cx));
                    this.refresh_menu(window, cx);
                }),
            )
            .on_click(cx.listener(|this, _, window, cx| this.open_copy_menu(window, cx)))
            .tooltip(move |_, cx| tooltip(tip.clone(), cx))
            .child("Copy options")
            .child(copy_ui::measure(Rc::clone(&self.options_at)));
        let mut wrapper = div().relative().flex_none().child(button);
        if let Some(menu) = self.menu.entity() {
            wrapper = wrapper.child(menu);
        }
        wrapper.into_any_element()
    }

    /// The items the menu shows now.
    fn menu_items(&self, cx: &Context<Self>) -> Vec<MenuItem> {
        let transfers = self.transfers.read(cx);
        copy_menu_items(
            transfers.forks_on(),
            transfers.system_items_on(),
            self.listing.read(cx).resource_forks(),
        )
    }

    /// Opens the menu above the button: it sits at the foot of the window.
    fn open_copy_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = self.menu_items(cx);
        let at = self.options_at.get();
        let height = metrics::MENU_ITEM_HEIGHT * items.len() as f32 + space::S8 * 2.0;
        let position = point(at.left(), at.top() - height - space::S4);
        let focus = self.options_focus.clone();
        self.menu.show(
            items,
            position,
            Some(focus),
            window,
            cx,
            |this, _, _, cx| {
                this.menu.clear();
                cx.notify();
            },
        );
    }

    /// A switch was turned over: the open menu shows the new checks.
    fn refresh_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.menu.entity() {
            let items = self.menu_items(cx);
            menu.update(cx, |menu, cx| menu.set_items(items, window, cx));
        }
    }
}

impl StatusBar {
    /// "4 hidden": shows the folder's hidden items until the user leaves it. A second press hides
    /// them again. The tooltip names the two settings.
    fn hidden_button(&self, text: String, revealed: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = Theme::get(cx).colors;
        let tip = SharedString::new_static(HIDDEN_TIP);
        div()
            .id("status-hidden")
            .key_context(BUTTON)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.toggle(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            .flex_none()
            .px(space::S4)
            .rounded(radius::ROW)
            .border_1()
            .border_color(gpui::transparent_black())
            .cursor_pointer()
            .text_color(if revealed { colors.ink } else { colors.ink2 })
            .hover(|style| style.bg(colors.hover))
            .focus(move |style| style.border_color(colors.ink))
            .tooltip(move |_, cx| tooltip(tip.clone(), cx))
            .child(SharedString::from(text))
            .into_any_element()
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.listing
            .update(cx, |listing, cx| listing.toggle_reveal(cx));
    }
}

/// What the footer says about the search index: an [`IndexState`] without the index itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexNote {
    /// Built; `skipped` structures could not be read (0 when complete).
    Ready { skipped: u64 },
    /// Being built; this many entries scanned so far.
    Building { entries: u64 },
    /// Could not be built. The text is a full sentence for the user.
    Failed(String),
}

impl IndexNote {
    /// The note for an index state; `None` while the source is only queued.
    fn of(state: &IndexState) -> Option<Self> {
        match state {
            IndexState::Absent => None,
            IndexState::Building { entries } => Some(Self::Building { entries: *entries }),
            IndexState::Ready(index) => Some(Self::Ready {
                skipped: index.summary().skipped,
            }),
            IndexState::Failed(reason) => Some(Self::Failed(reason.to_string())),
        }
    }

    /// The words in the footer. A failure is shown as given: `Search` words it as a sentence
    /// ("This volume could not be indexed (...), so it cannot be searched.") that already says
    /// search is unavailable and why, so it is not wrapped in parentheses a second time.
    fn text(&self) -> String {
        match self {
            Self::Ready { skipped: 0 } => "index up to date".to_owned(),
            Self::Ready { skipped } => {
                format!(
                    "index incomplete ({} unreadable)",
                    format::grouped(*skipped)
                )
            }
            Self::Building { entries } => {
                let items = format::items(usize::try_from(*entries).unwrap_or(usize::MAX));
                format!("indexing\u{2026} {items}")
            }
            Self::Failed(reason) => reason.clone(),
        }
    }
}

/// The pieces of the footer, left to right: "1,204 items", then "3 selected" when any row is
/// selected (with their total size when a folder is among them, "3 selected, 4.2 GB"), then the state of the volume's search index when there is one to report.
fn parts(
    len: usize,
    selected: usize,
    total: Option<u64>,
    index: Option<&IndexNote>,
) -> Vec<String> {
    let mut parts = vec![format::items(len)];
    if selected > 0 {
        let count = format::grouped(selected as u64);
        parts.push(match total {
            Some(bytes) => format!("{count} selected, {}", format::size(bytes)),
            None => format!("{count} selected"),
        });
    }
    parts.extend(index.map(IndexNote::text));
    parts
}

/// The words of the "hidden" button: how many items the two switches leave out of this folder,
/// or, while the folder shows them anyway, how many it is showing. `None` when there are none.
fn hidden_text(hidden: usize, revealed: bool) -> Option<String> {
    (hidden > 0).then(|| {
        let count = format::grouped(hidden as u64);
        if revealed {
            format!("{count} hidden, shown")
        } else {
            format!("{count} hidden")
        }
    })
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        // A running copy takes the line over, then the notice of the last one.
        let (running, notice) = {
            let transfers = self.transfers.read(cx);
            (transfers.running_view(), transfers.notice().cloned())
        };
        let frame = |row: Div| {
            div()
                .flex_none()
                .w_full()
                .flex()
                .flex_col()
                .child(self.strip.clone())
                .child(self.panel.clone())
                .child(row)
        };
        let line = || {
            div()
                .relative()
                .flex_none()
                .w_full()
                .h(size::CONTROL)
                .flex()
                .flex_row()
                .items_center()
                .gap(space::S16)
                .pl(metrics::ROW_INSET + metrics::ROW_PADDING)
                .pr(metrics::ROW_INSET + metrics::ROW_PADDING)
                .text_size(text::SMALL.size)
                .text_color(colors.ink3)
                .whitespace_nowrap()
                .overflow_hidden()
        };
        if let Some((words, fraction)) = running {
            let mut row = line()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(colors.ink2)
                        .child(SharedString::from(words)),
                )
                .child(self.cancel_button(&colors, cx));
            if let Some(fraction) = fraction {
                // Ink on the footer's edge, never the accent; it overlays the line, so the line
                // keeps its height when the bar comes and goes.
                row = row.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h(metrics::DISK_BAR_HEIGHT)
                        .w(relative(fraction))
                        .bg(colors.ink),
                );
            }
            return frame(row);
        }
        if let Some(notice) = notice {
            let children = self.notice_row(&notice, &colors, cx);
            return frame(line().children(children));
        }

        let listing = self.listing.read(cx);
        let copy_offered = listing.location().is_some()
            && listing.source_facts(cx).is_some_and(|facts| !facts.host);
        let hidden = listing.hidden_count();
        let revealed = listing.revealed();
        let parts = if listing.location().is_some() {
            let note = self
                .search
                .read(cx)
                .current_index_state(cx)
                .and_then(IndexNote::of);
            let selected = listing.selected_count();
            let total = if selected > 0 {
                listing.selection_total()
            } else {
                None
            };
            parts(listing.len(), selected, total, note.as_ref())
        } else {
            vec![BLANK_LINE.to_string()]
        };

        let mut children = parts
            .into_iter()
            .enumerate()
            .flat_map(|(at, part)| {
                let mut out = vec![div().child(SharedString::from(part)).into_any_element()];
                if at == 0
                    && let Some(text) = hidden_text(hidden, revealed)
                {
                    out.push(div().child("\u{b7}").into_any_element());
                    out.push(self.hidden_button(text, revealed, cx));
                }
                out
            })
            .collect::<Vec<_>>();
        // What a copy to this computer includes: only where a copy can start.
        if copy_offered {
            children.push(div().flex_1().into_any_element());
            children.push(self.options_button(&colors, cx));
        }
        frame(line().children(children))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (label, checked, usable, stays open) of each action row.
    fn rows_of(items: &[MenuItem]) -> Vec<(String, Option<bool>, bool, bool)> {
        use crate::components::menu::Enabled;
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action {
                    label,
                    checked,
                    enabled,
                    keep_open,
                    ..
                } => Some((
                    label.to_string(),
                    *checked,
                    matches!(enabled, Enabled::Yes),
                    *keep_open,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_copy_menu_has_the_two_switches_and_shows_their_state() {
        let rows = rows_of(&copy_menu_items(true, false, true));
        assert_eq!(
            rows,
            [
                (
                    "Keep resource forks and Finder information".to_owned(),
                    Some(true),
                    true,
                    true
                ),
                (
                    "Also copy housekeeping files".to_owned(),
                    Some(false),
                    true,
                    true
                ),
            ]
        );
        let rows = rows_of(&copy_menu_items(false, true, true));
        assert_eq!((rows[0].1, rows[1].1), (Some(false), Some(true)));
    }

    #[test]
    fn the_fork_switch_is_greyed_on_a_volume_without_forks() {
        let rows = rows_of(&copy_menu_items(false, false, false));
        assert!(!rows[0].2, "the fork switch cannot be chosen");
        // The other switch still works.
        assert!(rows[1].2);
    }

    #[test]
    fn the_hidden_button_says_how_many() {
        assert_eq!(hidden_text(0, false), None);
        assert_eq!(hidden_text(0, true), None);
        assert_eq!(hidden_text(4, false).as_deref(), Some("4 hidden"));
        assert_eq!(hidden_text(1_200, false).as_deref(), Some("1,200 hidden"));
        assert_eq!(hidden_text(4, true).as_deref(), Some("4 hidden, shown"));
    }

    #[test]
    fn items_alone_when_nothing_is_selected() {
        assert_eq!(parts(0, 0, None, None), ["0 items"]);
        assert_eq!(parts(1, 0, None, None), ["1 item"]);
        assert_eq!(parts(1_204, 0, None, None), ["1,204 items"]);
    }

    #[test]
    fn a_selected_folder_adds_the_total() {
        assert_eq!(
            parts(17, 3, Some(4_509_715_661), None),
            ["17 items", "3 selected, 4.2 GB"]
        );
        assert_eq!(parts(17, 0, Some(5), None), ["17 items"]);
    }

    #[test]
    fn selection_count_follows_the_items() {
        assert_eq!(parts(17, 1, None, None), ["17 items", "1 selected"]);
        assert_eq!(
            parts(5_000, 1_200, None, None),
            ["5,000 items", "1,200 selected"]
        );
    }

    #[test]
    fn a_complete_index_is_up_to_date() {
        let note = IndexNote::Ready { skipped: 0 };
        assert_eq!(
            parts(3, 0, None, Some(&note)),
            ["3 items", "index up to date"]
        );
    }

    #[test]
    fn an_incomplete_index_counts_what_was_unreadable() {
        let note = IndexNote::Ready { skipped: 1_234 };
        assert_eq!(
            parts(3, 0, None, Some(&note)),
            ["3 items", "index incomplete (1,234 unreadable)"]
        );
        let one = IndexNote::Ready { skipped: 1 };
        assert_eq!(one.text(), "index incomplete (1 unreadable)");
    }

    #[test]
    fn a_building_index_counts_the_items_so_far() {
        let note = IndexNote::Building { entries: 312_000 };
        assert_eq!(
            parts(40, 0, None, Some(&note)),
            ["40 items", "indexing\u{2026} 312,000 items"]
        );
        assert_eq!(
            IndexNote::Building { entries: 0 }.text(),
            "indexing\u{2026} 0 items"
        );
    }

    #[test]
    fn a_failed_index_shows_its_sentence_as_given() {
        let reason = "This volume could not be indexed (read error), so it cannot be searched.";
        let note = IndexNote::Failed(reason.to_owned());
        assert_eq!(
            parts(2, 1, None, Some(&note)),
            ["2 items", "1 selected", reason]
        );
    }

    #[test]
    fn the_index_note_comes_after_the_selection() {
        let note = IndexNote::Ready { skipped: 0 };
        assert_eq!(
            parts(9, 2, None, Some(&note)),
            ["9 items", "2 selected", "index up to date"]
        );
    }

    #[test]
    fn the_states_map_to_notes() {
        assert_eq!(IndexNote::of(&IndexState::Absent), None);
        assert_eq!(
            IndexNote::of(&IndexState::Building { entries: 5 }),
            Some(IndexNote::Building { entries: 5 })
        );
        assert_eq!(
            IndexNote::of(&IndexState::Failed("nope".into())),
            Some(IndexNote::Failed("nope".to_owned()))
        );
    }
}
