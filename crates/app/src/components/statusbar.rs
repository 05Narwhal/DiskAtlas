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
//! search index of the volume stands.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*,
};

use crate::components::sidebar::tooltip;
use crate::format;
use crate::keymap::{BUTTON, controls};
use crate::state::listing::Listing;
use crate::state::search::{IndexState, Search, SearchEvent};
use crate::state::visibility::HIDDEN_TIP;
use crate::theme::{
    Theme, metrics,
    scale::{radius, space, text},
};

/// A no-break space: gives an empty footer the height of a line of text.
const BLANK_LINE: &str = "\u{a0}";

/// The footer line under the file list.
pub struct StatusBar {
    listing: Entity<Listing>,
    search: Entity<Search>,
    /// The "hidden" button.
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl StatusBar {
    /// Reports on the rows and the selection of `listing`, and on the index state `search` holds.
    pub fn new(listing: Entity<Listing>, search: Entity<Search>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&listing, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, _: &SearchEvent, cx| cx.notify()),
        ];
        Self {
            listing,
            search,
            focus: cx.focus_handle().tab_stop(true),
            _subscriptions: subscriptions,
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
        let listing = self.listing.read(cx);
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

        div()
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .gap(space::S16)
            .pl(metrics::ROW_INSET + metrics::ROW_PADDING)
            .py(space::S8)
            .text_size(text::SMALL.size)
            .text_color(colors.ink3)
            .whitespace_nowrap()
            .overflow_hidden()
            .children(
                parts
                    .into_iter()
                    .enumerate()
                    .flat_map(|(at, part)| {
                        let mut out =
                            vec![div().child(SharedString::from(part)).into_any_element()];
                        if at == 0
                            && let Some(text) = hidden_text(hidden, revealed)
                        {
                            out.push(div().child("\u{b7}").into_any_element());
                            out.push(self.hidden_button(text, revealed, cx));
                        }
                        out
                    })
                    .collect::<Vec<_>>(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
