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

//! The details pane: what the app knows about the selected file or folder.
//!
//! It reads the [`Listing`] and never changes it. The facts the listing already carries (name,
//! kind, size, modified) show at once; the rest comes from `FileSystem::metadata`, which runs on the
//! background executor when the selected node changes and is dropped if the selection moved on.
//!
//! The preview well shows what `QuickLook` holds for the selection (it follows the selection while
//! the pane is visible, with the panel closed too), scaled down. The same file holds the rows of
//! Quick Look's Details tab, which say the same things in technical words.

use std::sync::Arc;

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{EntryFlags, NodeKind, Stream, Timestamp};
use disk_atlas_lib::search::kind::classify_entry;
use gpui::{
    Context, Div, Entity, FontWeight, ImageSource, IntoElement, ObjectFit, Render, SharedString,
    StyledImage, Subscription, Task, Window, div, img, prelude::*, svg,
};

use crate::format;
use crate::preview::Preview;
use crate::state::facts::{Extra, facts, journal_text, selection_text, subtitle};
use crate::state::listing::{Listing, ReachKey};
use crate::state::quick_look::{LoadState, QuickLook};
use crate::state::sources::SourceId;
use crate::theme::{
    Colors, Glyph, Theme, metrics,
    scale::{radius, space, text},
};

/// The selected row: the source it belongs to and which row it is there (a node, or a hit of a
/// search whose ids are not the filesystem's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    source: SourceId,
    row: ReachKey,
}

/// The selected entry, copied out of the listing so rendering holds no borrow of it.
struct Subject {
    name: SharedString,
    kind: NodeKind,
    size: Option<u64>,
    modified: Option<Timestamp>,
    browsable: bool,
}

/// What the pane shows.
enum Content {
    One(Subject, Option<Extra>),
    Many(usize),
    /// The folder shown: its name, its item count and the journal row's text, if any.
    Folder(SharedString, usize, Option<String>),
    Nothing,
}

/// The details pane: facts about the selected item.
pub struct Details {
    listing: Entity<Listing>,
    /// Holds the preview the well shows.
    quick_look: Entity<QuickLook>,
    /// The node whose metadata is held in `extra` or being fetched.
    requested: Option<Key>,
    /// The metadata that arrived, with the node it belongs to.
    extra: Option<(Key, Extra)>,
    /// Replacing the task drops the previous one.
    task: Option<Task<()>>,
    _subscriptions: [Subscription; 2],
}

impl Details {
    /// Shows facts about the selection in `listing`.
    pub fn new(
        listing: Entity<Listing>,
        quick_look: Entity<QuickLook>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&listing, |this, _listing, cx| {
            this.follow_selection(cx);
            cx.notify();
        });
        let preview_subscription = cx.observe(&quick_look, |_, _, cx| cx.notify());
        let mut details = Self {
            listing,
            quick_look,
            requested: None,
            extra: None,
            task: None,
            _subscriptions: [subscription, preview_subscription],
        };
        details.follow_selection(cx);
        details
    }

    /// Starts fetching metadata when the selected node is not the one already requested.
    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let selected = {
            let listing = self.listing.read(cx);
            match (listing.location(), listing.primary_reach()) {
                (Some(location), Some(reach)) => Some((
                    Key {
                        source: location.source(),
                        row: reach.key(),
                    },
                    reach,
                    Arc::clone(location.fs()),
                )),
                _ => None,
            }
        };
        let key = selected.as_ref().map(|(key, _, _)| *key);
        if key == self.requested {
            return;
        }
        self.requested = key;
        self.extra = None;
        self.task = None;
        let Some((key, reach, fs)) = selected else {
            return;
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            // A hit on a host folder is walked to its real id first; all of it is disk work.
            let result = cx
                .background_executor()
                .spawn({
                    let reach = reach.clone();
                    async move {
                        match reach.resolve(fs.as_ref())? {
                            Some(node) => Ok(Some((node, fs.metadata(node)?))),
                            None => Ok::<_, Error>(None),
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                // The selection may have moved on while the filesystem answered.
                if this.requested != Some(key) {
                    return;
                }
                match result {
                    Ok(Some((node, metadata))) => {
                        this.listing
                            .update(cx, |listing, _| listing.remember_resolved(&reach, node));
                        let extra = Extra {
                            created: metadata.created,
                            accessed: metadata.accessed,
                            link_target: metadata.link_target,
                            resource_size: metadata.resource_size,
                            finder_codes: metadata.finder_codes,
                            owner: metadata.owner,
                            attributes: metadata.attributes,
                            compressed: metadata.flags.contains(EntryFlags::COMPRESSED),
                        };
                        this.extra = Some((key, extra));
                        cx.notify();
                    }
                    Ok(None) => tracing::debug!("details: the selected hit no longer exists"),
                    Err(error) => tracing::debug!(%error, "details: metadata unavailable"),
                }
            })
            .ok();
        }));
    }

    fn content(&self, cx: &Context<Self>) -> Content {
        let listing = self.listing.read(cx);
        if let Some(entry) = listing.primary() {
            let subject = Subject {
                name: SharedString::from(entry.name.to_owned()),
                kind: entry.kind,
                size: entry.size,
                modified: entry.modified,
                browsable: entry.is_browsable(),
            };
            let key = listing
                .location()
                .zip(listing.primary_reach())
                .map(|(location, reach)| Key {
                    source: location.source(),
                    row: reach.key(),
                });
            let extra = self
                .extra
                .as_ref()
                .filter(|(held, _)| key == Some(*held))
                .map(|(_, extra)| extra.clone());
            return Content::One(subject, extra);
        }
        let selected = listing.selected_count();
        if selected > 1 {
            return Content::Many(selected);
        }
        match listing.location() {
            Some(location) => Content::Folder(
                location.name().clone(),
                listing.len(),
                listing.journal().and_then(journal_text),
            ),
            None => Content::Nothing,
        }
    }
}

/// Label and value rows in two columns, as the details pane lays out its facts.
fn fact_rows(rows: Vec<(&'static str, SharedString)>, colors: &Colors) -> Div {
    let ink3 = colors.ink3;
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(space::S8)
        .text_size(text::SMALL.size)
        .children(rows.into_iter().map(move |(label, value)| {
            div()
                .flex()
                .flex_row()
                .gap(metrics::ROW_PADDING)
                .child(
                    div()
                        .flex_none()
                        .w(metrics::DETAILS_LABEL_WIDTH)
                        .text_color(ink3)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(value))
        }))
}

/// A name in bold that wraps instead of being cut off, with a line of secondary text under it.
fn heading(name: SharedString, subtitle: String, colors: &Colors) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(space::S4)
        .child(
            div()
                .min_w_0()
                .text_size(text::HEADING.size)
                .font_weight(FontWeight::BOLD)
                .child(name),
        )
        .child(
            div()
                .min_w_0()
                .text_color(colors.ink3)
                .child(SharedString::from(subtitle)),
        )
}

impl Details {
    /// The well above the name: the selection's picture or first lines when `QuickLook` has them
    /// ready, otherwise the kind's icon.
    fn preview_well(&self, glyph: Glyph, colors: &Colors, cx: &Context<Self>) -> Div {
        let quick_look = self.quick_look.read(cx);
        // A resource fork has no preview; show the file's own.
        let preview = match quick_look.state() {
            LoadState::Ready(preview) if quick_look.stream() == Stream::Data => Some(preview),
            _ => None,
        };
        let well = |height| {
            div()
                .flex_none()
                .w_full()
                .h(height)
                .rounded(radius::WELL)
                .bg(colors.well)
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
        };
        match preview {
            Some(Preview::Image(image)) => well(metrics::PREVIEW_WELL_HEIGHT).p(space::S8).child(
                img(ImageSource::Render(image.image.clone()))
                    .object_fit(ObjectFit::Contain)
                    .size_full(),
            ),
            Some(Preview::Text(text)) if !text.text().is_empty() => {
                let lines = (0..text.row_count().min(metrics::PEEK_LINES))
                    .filter_map(|index| text.row(index))
                    .map(|row| {
                        div()
                            .flex_none()
                            .w_full()
                            .h(metrics::PEEK_LINE_HEIGHT)
                            .truncate()
                            .child(display_line(row))
                    });
                well(metrics::PEEK_TEXT_WELL_HEIGHT)
                    .items_start()
                    .justify_start()
                    .flex_col()
                    .p(space::S8)
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink2)
                    .children(lines)
            }
            _ => well(metrics::PREVIEW_WELL_HEIGHT).child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(metrics::DETAILS_KIND_TILE)
                    .rounded(radius::FIELD)
                    .bg(colors.wash(glyph))
                    .child(
                        svg()
                            .path(glyph.path())
                            .flex_none()
                            .size(metrics::DETAILS_KIND_ICON)
                            .text_color(colors.tint(glyph)),
                    ),
            ),
        }
    }
}

impl Render for Details {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let pane = div()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .p(space::S16)
            .gap(space::S16);

        match self.content(cx) {
            Content::One(subject, extra) => {
                let glyph =
                    Glyph::from_class(classify_entry(&subject.name, subject.browsable, None));
                let well = self.preview_well(glyph, &colors, cx);
                let rows = facts(subject.modified, extra.as_ref());
                let compressed = extra.as_ref().is_some_and(|extra| extra.compressed);
                let codes = extra.as_ref().and_then(|extra| extra.finder_codes);
                let line = subtitle(
                    &subject.name,
                    subject.kind,
                    codes,
                    subject.size,
                    subject.browsable,
                    compressed,
                );
                pane.child(well)
                    .child(heading(subject.name, line, &colors))
                    .child(fact_rows(rows, &colors))
            }
            Content::Many(count) => pane
                .items_center()
                .justify_center()
                .text_color(colors.ink3)
                .child(SharedString::from(selection_text(count))),
            Content::Folder(name, count, journal) => {
                let rows: Vec<_> = journal
                    .into_iter()
                    .map(|text| ("Journal", SharedString::from(text)))
                    .collect();
                pane.child(heading(name, format::items(count), &colors))
                    .child(fact_rows(rows, &colors))
            }
            Content::Nothing => pane,
        }
    }
}

/// A line of text with tabs as spaces, since one glyph cannot be a tab stop.
pub fn display_line(row: &str) -> SharedString {
    SharedString::from(row.replace('\t', "    "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_become_spaces_in_a_line() {
        assert_eq!(display_line("a	b"), "a    b");
        assert_eq!(display_line("plain"), "plain");
    }
}
