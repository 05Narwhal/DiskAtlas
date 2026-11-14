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

//! The Quick Look entity: which file is shown, its tab, and the preview cache.
//!
//! It follows the listing's one selected file. When that changes while the
//! panel is open it cancels the load in flight, waits [`DEBOUNCE`] of quiet
//! (holding Down must not start a load per row), and builds the preview on the
//! background executor with `preview::load`. Every result carries the
//! generation it was started under; a result for a file the user has left is
//! dropped. Design: `.claude/docs/design/m5-quicklook-copy.md` §3.
//!
//! The panel (M5 task 6) reads the state through the accessors and calls
//! [`QuickLook::toggle`], [`QuickLook::close`], [`QuickLook::set_tab`] and
//! [`QuickLook::set_stream`]. The methods that can drop a picture from the
//! cache take the `Window` they run in when they have one: GPUI keeps a
//! texture per painted image, and the window being updated is the one place
//! `App::drop_image` cannot reach by itself.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{Extent, FileSystem, Metadata, NodeId, NodeKind, Stream, XattrInfo};
use gpui::{Context, Entity, RenderImage, SharedString, Subscription, Task, Window};

use crate::preview::{self, Decoder, Preview, Subject};
use crate::state::listing::{Listing, Motion, Reach, ReachKey};
use crate::state::sources::SourceId;

/// Quiet time after the selection moves before a load starts.
pub const DEBOUNCE: Duration = Duration::from_millis(30);

/// How long a load runs before the panel says "Loading…", like listings.
pub const LOADING_DELAY: Duration = Duration::from_millis(150);

/// Most previews the cache keeps.
pub const PREVIEW_CACHE: usize = 8;

/// Most bytes (by `Preview::cost`) the cache keeps.
pub const PREVIEW_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// What the panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// The file's content, as text or as a picture.
    #[default]
    Preview,
    /// The bytes of the stream.
    Bytes,
    /// Everything the volume records about the file.
    Details,
}

impl Tab {
    /// The tab after this one, or before it, wrapping round.
    pub fn cycle(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Preview, true) | (Self::Details, false) => Self::Bytes,
            (Self::Bytes, true) | (Self::Preview, false) => Self::Details,
            (Self::Details, true) | (Self::Bytes, false) => Self::Preview,
        }
    }
}

/// The tab to show after the fork switch picked `stream`: a resource fork has no preview in M5,
/// so the panel moves from Preview to Bytes. Other tabs stay.
pub fn tab_after_fork_switch(stream: Stream, tab: Tab) -> Tab {
    if stream == Stream::Resource && tab == Tab::Preview {
        Tab::Bytes
    } else {
        tab
    }
}

/// True when the panel offers the "Data | Resource" switch: the volume has resource forks and
/// this file's is not empty.
pub fn fork_switch_visible(volume_has_forks: bool, resource_size: Option<u64>) -> bool {
    volume_has_forks && resource_size.is_some_and(|size| size > 0)
}

/// Where a load stands.
#[derive(Clone)]
pub enum LoadState<T> {
    /// Nothing to load: the panel is closed or no single file is selected.
    Idle,
    /// A load is pending or running. The panel shows "Loading…" only when
    /// [`QuickLook::show_loading`] says so.
    Loading,
    /// The result.
    Ready(T),
    /// The load failed. The text says why, for the user.
    Failed(SharedString),
}

/// What the Details tab shows, read on the loading thread.
#[derive(Debug, Clone)]
pub struct FileDetails {
    /// Everything the volume records about the file.
    pub metadata: Metadata,
    /// The stream the extents are of.
    pub stream: Stream,
    /// Extended attributes: names and sizes. The error says why they are missing.
    pub xattrs: Result<Vec<XattrInfo>, SharedString>,
    /// Where the stream lies on the volume.
    pub extents: Result<Vec<Extent>, SharedString>,
    /// The volume records local time: print the dates as stored.
    pub local_times: bool,
}

/// What to read besides the file's identity.
#[derive(Debug, Clone, Copy)]
struct Ask {
    /// The preview is not in the cache.
    preview: bool,
    /// The panel is open: read the Details tab's data.
    details: bool,
}

/// What a load brings back.
struct Loaded {
    /// The file's real id, `None` when it could not be found.
    node: Option<NodeId>,
    reach: Reach,
    /// `None` when the preview was not asked for.
    preview: Option<anyhow::Result<Option<Preview>>>,
    /// `None` when the details were not asked for.
    details: Option<Result<FileDetails, SharedString>>,
    /// The fork switch should show.
    resource_fork: bool,
}

/// The sentence for a part of the details that could not be read.
fn part_error(error: &Error) -> SharedString {
    match error {
        Error::Unsupported(_) => "This volume does not say.".into(),
        other => format!("Could not be read ({other}).").into(),
    }
}

/// Reads the file's identity, then what `ask` wants. Blocking: runs on the background executor.
fn load_file(
    mut subject: Subject,
    reach: Reach,
    decoder: &Decoder,
    cancel: &AtomicBool,
    ask: Ask,
) -> Loaded {
    let node = match reach.resolve(subject.fs.as_ref()) {
        Ok(Some(node)) => node,
        found => {
            let error = match found {
                Ok(_) => anyhow::anyhow!("This file no longer exists."),
                Err(error) => error.into(),
            };
            let text = SharedString::from(error.to_string());
            return Loaded {
                node: None,
                reach,
                preview: ask.preview.then(|| Err(error)),
                details: ask.details.then_some(Err(text)),
                resource_fork: false,
            };
        }
    };
    subject.node = node;
    // The listing has no type code or volume format; the engine is asked here, off the UI thread.
    let metadata = subject.fs.metadata(node);
    let info = subject.fs.info();
    subject.finder_codes = metadata.as_ref().ok().and_then(|m| m.finder_codes);
    subject.format = info.format;
    let resource_fork = metadata
        .as_ref()
        .is_ok_and(|m| fork_switch_visible(info.resource_forks, m.resource_size));
    let preview = ask
        .preview
        .then(|| preview::load(&subject, decoder, cancel));
    let details = ask.details.then(|| match metadata {
        Ok(metadata) => Ok(FileDetails {
            xattrs: subject.fs.xattrs(node).map_err(|error| part_error(&error)),
            extents: subject
                .fs
                .extents(node, subject.stream)
                .map_err(|error| part_error(&error)),
            stream: subject.stream,
            local_times: info.local_times,
            metadata,
        }),
        Err(error) => Err(part_error(&error)),
    });
    Loaded {
        node: Some(node),
        reach,
        preview,
        details,
        resource_fork,
    }
}

/// What the cache is keyed by.
type Key = (SourceId, NodeId, Stream);

/// The last few previews, so stepping back is instant. Plain data, so its
/// rules are testable without GPUI.
struct PreviewCache {
    /// Least recently used first.
    entries: VecDeque<(Key, Preview)>,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl PreviewCache {
    fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    /// The preview for `key`, which becomes the most recently used.
    fn get(&mut self, key: &Key) -> Option<Preview> {
        let at = self.entries.iter().position(|(held, _)| held == key)?;
        let entry = self.entries.remove(at)?;
        let preview = entry.1.clone();
        self.entries.push_back(entry);
        Some(preview)
    }

    /// Keeps `preview` under `key` as the most recently used. One that alone
    /// costs more than the byte cap is not kept. Returns what was pushed out
    /// (the old preview of `key` included).
    fn insert(&mut self, key: Key, preview: &Preview) -> Vec<Preview> {
        let mut out = Vec::new();
        if let Some(at) = self.entries.iter().position(|(held, _)| *held == key)
            && let Some((_, old)) = self.entries.remove(at)
        {
            self.bytes = self.bytes.saturating_sub(old.cost());
            out.push(old);
        }
        let cost = preview.cost();
        if cost > self.max_bytes {
            return out;
        }
        self.entries.push_back((key, preview.clone()));
        self.bytes = self.bytes.saturating_add(cost);
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some((_, old)) = self.entries.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.cost());
            out.push(old);
        }
        out
    }

    /// True when the cache holds a preview of this very picture.
    fn holds_image(&self, image: &Arc<RenderImage>) -> bool {
        self.entries.iter().any(|(_, held)| match held {
            Preview::Image(held) => Arc::ptr_eq(&held.image, image),
            _ => false,
        })
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<Key> {
        self.entries.iter().map(|(key, _)| *key).collect()
    }
}

/// Tells loads apart: starting one cancels the one before it, and only the
/// newest one's result may be applied.
struct Gate {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

impl Gate {
    fn new() -> Self {
        Self {
            generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Cancels the load in flight, if any, and starts a new generation with
    /// a fresh cancel flag.
    fn restart(&mut self) -> (u64, Arc<AtomicBool>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.generation += 1;
        self.cancel = Arc::new(AtomicBool::new(false));
        (self.generation, Arc::clone(&self.cancel))
    }

    /// True when a result of `generation` is still wanted.
    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }
}

/// The selected file, as far as the listing knows it.
struct Wanted {
    source: SourceId,
    /// How to reach the file's real id; a hit on a host folder is walked on the loading thread.
    reach: Reach,
    name: SharedString,
    fs: Arc<dyn FileSystem>,
}

/// The sentence for a failed load: the engine's errors are wrapped, the
/// providers' own sentences are shown as they are.
fn message(error: &anyhow::Error) -> SharedString {
    if error.downcast_ref::<Error>().is_some() {
        format!("This file could not be read ({error}).").into()
    } else {
        error.to_string().into()
    }
}

/// The file shown by Quick Look, and how.
pub struct QuickLook {
    listing: Entity<Listing>,
    /// Made once, on the UI thread, and cloned into every load.
    decoder: Arc<Decoder>,
    open: bool,
    tab: Tab,
    stream: Stream,
    subject: Option<Subject>,
    /// The file `subject` is of, to tell a move to another file from a
    /// notification that changed nothing.
    target: Option<(SourceId, ReachKey)>,
    preview: LoadState<Preview>,
    /// The Details tab's data, loaded with the preview while the panel is open.
    details: LoadState<Arc<FileDetails>>,
    /// The file has a resource fork the panel can switch to.
    resource_fork: bool,
    /// The details pane shows a small preview, so the selection is followed with the panel
    /// closed too.
    peek: bool,
    /// Set by the 150 ms timer when the load has not finished.
    loading_visible: bool,
    gate: Gate,
    cache: PreviewCache,
    load_task: Option<Task<()>>,
    loading_timer: Option<Task<()>>,
    _subscription: Subscription,
}

impl QuickLook {
    /// Follows the selection of `listing`. Closed until [`QuickLook::toggle`].
    pub fn new(listing: Entity<Listing>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&listing, |this, _listing, cx| this.follow(cx));
        Self {
            listing,
            decoder: preview::decoder(cx),
            open: false,
            tab: Tab::default(),
            stream: Stream::Data,
            subject: None,
            target: None,
            preview: LoadState::Idle,
            details: LoadState::Idle,
            resource_fork: false,
            peek: false,
            loading_visible: false,
            gate: Gate::new(),
            cache: PreviewCache::new(PREVIEW_CACHE, PREVIEW_CACHE_BYTES),
            load_task: None,
            loading_timer: None,
            _subscription: subscription,
        }
    }

    /// True while the panel is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The tab the panel shows.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// The stream shown: the data fork or the resource fork.
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// The file shown, when the panel is open and one file is selected. Its
    /// `finder_codes` and `format` are not filled in: they are read on the
    /// loading thread and used only there.
    pub fn subject(&self) -> Option<&Subject> {
        self.subject.as_ref()
    }

    /// Where the preview stands.
    pub fn state(&self) -> &LoadState<Preview> {
        &self.preview
    }

    /// The Details tab's data.
    pub fn details(&self) -> &LoadState<Arc<FileDetails>> {
        &self.details
    }

    /// True when the file has a resource fork and the volume has forks: the panel shows the
    /// "Data | Resource" switch.
    pub fn has_resource_fork(&self) -> bool {
        self.resource_fork
    }

    /// True when the Details tab should say "Reading details…".
    pub fn show_details_loading(&self) -> bool {
        self.loading_visible && matches!(self.details, LoadState::Loading)
    }

    /// True when the panel should say "Loading…": the load has run for more
    /// than [`LOADING_DELAY`].
    pub fn show_loading(&self) -> bool {
        self.loading_visible && matches!(self.preview, LoadState::Loading)
    }

    /// The generation of the load the state belongs to.
    pub fn generation(&self) -> u64 {
        self.gate.generation
    }

    /// Moves the listing's cursor as an arrow key in the file list does, for the panel's own
    /// window, which has no list; the panel follows the new selection.
    pub fn move_selection(&mut self, motion: Motion, cx: &mut Context<Self>) {
        self.listing
            .update(cx, |listing, cx| listing.move_cursor(motion, false, cx));
    }

    /// Opens the panel, or closes it.
    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(window, cx);
        } else {
            self.open(cx);
        }
    }

    /// Starts or stops following the selection for the details pane's preview well, which needs
    /// no panel. Stopping while the panel is closed releases the picture; `window` is the
    /// window being updated, if the caller runs inside one.
    pub fn set_peek(&mut self, peek: bool, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if self.peek == peek {
            return;
        }
        self.peek = peek;
        if self.open {
            return;
        }
        if peek {
            self.target = None;
            self.follow(cx);
        } else {
            self.reset(window, cx);
        }
        cx.notify();
    }

    /// True while the panel is open or the details pane wants a preview.
    fn active(&self) -> bool {
        self.open || self.peek
    }

    /// Forgets the file: nothing is shown or loading.
    fn reset(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.target = None;
        self.subject = None;
        self.stop_loading();
        self.stream = Stream::Data;
        self.resource_fork = false;
        self.details = LoadState::Idle;
        self.set_preview(LoadState::Idle, window, cx);
    }

    /// Opens the panel on the selected file.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        // Nothing is shown yet, so there is no target to compare with.
        self.target = None;
        self.follow(cx);
        cx.notify();
    }

    /// Closes the panel and cancels the load. The cache stays, so opening
    /// again on the same file is instant.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.reset(Some(window), cx);
        // The details pane may still want the picture.
        if self.peek {
            self.follow(cx);
        }
        cx.notify();
    }

    /// Shows another tab.
    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// Shows the data fork or the resource fork of the file, reloading.
    pub fn set_stream(&mut self, stream: Stream, window: &mut Window, cx: &mut Context<Self>) {
        if self.stream == stream || !self.open {
            return;
        }
        self.stream = stream;
        let wanted = self.wanted(cx);
        // The selection and the target are in step unless the listing has
        // moved since the last notification, which `follow` will handle.
        if wanted.as_ref().map(|w| (w.source, w.reach.key())) == self.target {
            self.start(wanted, Some(window), cx);
        }
        cx.notify();
    }

    /// The one selected file of the listing, or `None` when the selection is
    /// not exactly one file (or a link, whose text is its target).
    fn wanted(&self, cx: &Context<Self>) -> Option<Wanted> {
        let listing = self.listing.read(cx);
        let location = listing.location()?;
        let entry = listing.primary()?;
        if !matches!(entry.kind, NodeKind::File | NodeKind::Symlink) {
            return None;
        }
        Some(Wanted {
            source: location.source(),
            reach: listing.primary_reach()?,
            name: SharedString::from(entry.name.to_owned()),
            fs: Arc::clone(location.fs()),
        })
    }

    /// The listing changed: when the selected file did, load it.
    fn follow(&mut self, cx: &mut Context<Self>) {
        if !self.active() {
            return;
        }
        let wanted = self.wanted(cx);
        let target = wanted.as_ref().map(|w| (w.source, w.reach.key()));
        if target == self.target {
            return;
        }
        self.target = target;
        // A fork is chosen for one file.
        self.stream = Stream::Data;
        self.resource_fork = false;
        // The listing's observer runs after the update that moved the
        // selection has finished, so no window is mid-update and
        // `drop_image` reaches them all.
        self.start(wanted, None, cx);
        cx.notify();
    }

    /// Cancels the load in flight and forgets its timers.
    fn stop_loading(&mut self) {
        self.gate.restart();
        self.load_task = None;
        self.loading_timer = None;
        self.loading_visible = false;
    }

    /// Shows `wanted` from the cache, or loads it.
    fn start(
        &mut self,
        wanted: Option<Wanted>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.stop_loading();
        let Some(wanted) = wanted else {
            self.subject = None;
            self.details = LoadState::Idle;
            self.set_preview(LoadState::Idle, window, cx);
            return;
        };
        // A hit on a host folder has no real id until the loading thread has walked its path;
        // `subject.node` holds a stand-in until `finish` puts the real one there.
        let subject = Subject {
            fs: wanted.fs,
            node: wanted.reach.provisional(),
            name: wanted.name,
            stream: self.stream,
            finder_codes: None,
            format: None,
        };
        self.subject = Some(subject.clone());
        let cached = wanted
            .reach
            .known()
            .and_then(|node| self.cache.get(&(wanted.source, node, self.stream)));
        let ask = Ask {
            preview: cached.is_none(),
            details: self.open,
        };
        self.details = if ask.details {
            LoadState::Loading
        } else {
            LoadState::Idle
        };
        self.set_preview(
            cached.map_or(LoadState::Loading, LoadState::Ready),
            window,
            cx,
        );
        if !ask.preview && !ask.details {
            return;
        }

        let source = wanted.source;
        let stream = self.stream;
        let reach = wanted.reach;
        let generation = self.gate.generation;
        let cancel = Arc::clone(&self.gate.cancel);
        let decoder = Arc::clone(&self.decoder);

        self.loading_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                let waiting = matches!(this.preview, LoadState::Loading)
                    || matches!(this.details, LoadState::Loading);
                if this.gate.is_current(generation) && waiting {
                    this.loading_visible = true;
                    cx.notify();
                }
            })
            .ok();
        }));

        self.load_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let loaded = cx
                .background_executor()
                .spawn(async move { load_file(subject, reach, decoder.as_ref(), &cancel, ask) })
                .await;
            this.update(cx, |this, cx| {
                // Kept even when the user has moved on: the next ask is then free.
                if let Some(node) = loaded.node {
                    this.listing.update(cx, |listing, _| {
                        listing.remember_resolved(&loaded.reach, node)
                    });
                }
                this.finish(generation, (source, stream), loaded, cx);
            })
            .ok();
        }));
    }

    /// Applies a load's result, unless the user has moved on. `of` is the source and stream the
    /// load was started for; the cache key takes the file's real id from the result.
    fn finish(
        &mut self,
        generation: u64,
        of: (SourceId, Stream),
        loaded: Loaded,
        cx: &mut Context<Self>,
    ) {
        if !self.gate.is_current(generation) {
            return;
        }
        let key = loaded.node.map(|node| (of.0, node, of.1));
        if let (Some((_, node, _)), Some(subject)) = (key, self.subject.as_mut()) {
            subject.node = node;
        }
        self.load_task = None;
        self.loading_timer = None;
        self.loading_visible = false;
        self.resource_fork = loaded.resource_fork;
        match loaded.details {
            Some(Ok(details)) => self.details = LoadState::Ready(Arc::new(details)),
            Some(Err(text)) => self.details = LoadState::Failed(text),
            None => {}
        }
        match loaded.preview {
            Some(Ok(Some(preview))) => {
                let evicted = key.map_or_else(Vec::new, |key| self.cache.insert(key, &preview));
                // Shown first, so what is on screen counts as in use when
                // the evicted ones are released.
                self.set_preview(LoadState::Ready(preview), None, cx);
                for old in evicted {
                    if let Preview::Image(image) = old {
                        self.release(image.image, None, cx);
                    }
                }
            }
            // Only a cancelled load answers this, and one that is still
            // wanted is not cancelled.
            Some(Ok(None)) => self.set_preview(LoadState::Idle, None, cx),
            Some(Err(error)) => {
                tracing::debug!(%error, "quick look: preview failed");
                self.set_preview(LoadState::Failed(message(&error)), None, cx);
            }
            // The preview came from the cache and is on screen.
            None => {}
        }
        cx.notify();
    }

    /// Replaces what is shown, releasing the old picture when nothing else
    /// holds it.
    fn set_preview(
        &mut self,
        next: LoadState<Preview>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let old = std::mem::replace(&mut self.preview, next);
        if let LoadState::Ready(Preview::Image(image)) = old {
            self.release(image.image, window, cx);
        }
    }

    /// Drops `image`'s texture from every window, unless the cache or the
    /// panel still holds it. `window` is the window being updated, if the
    /// caller runs inside one: GPUI takes that window out of its list for the
    /// duration, so `drop_image` must be told about it.
    fn release(
        &mut self,
        image: Arc<RenderImage>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let shown = matches!(
            &self.preview,
            LoadState::Ready(Preview::Image(shown)) if Arc::ptr_eq(&shown.image, &image)
        );
        if shown || self.cache.holds_image(&image) {
            return;
        }
        cx.drop_image(image, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::TextPreview;

    fn key(n: u32) -> Key {
        (SourceId::for_test(1), NodeId(u64::from(n)), Stream::Data)
    }

    /// A text preview of about `bytes` bytes.
    fn text(bytes: usize) -> Preview {
        Preview::Text(TextPreview::new("x".repeat(bytes), "UTF-8", false))
    }

    #[test]
    fn cache_keeps_the_last_entries_and_evicts_the_oldest() {
        let mut cache = PreviewCache::new(3, u64::MAX);
        for n in 1..=3 {
            assert!(cache.insert(key(n), &text(10)).is_empty());
        }
        assert_eq!(cache.keys(), [key(1), key(2), key(3)]);
        let evicted = cache.insert(key(4), &text(10));
        assert_eq!(evicted.len(), 1);
        assert_eq!(cache.keys(), [key(2), key(3), key(4)]);
        assert!(cache.get(&key(1)).is_none());
    }

    #[test]
    fn cache_get_makes_the_entry_the_newest() {
        let mut cache = PreviewCache::new(3, u64::MAX);
        for n in 1..=3 {
            cache.insert(key(n), &text(10));
        }
        assert!(cache.get(&key(1)).is_some());
        assert_eq!(cache.keys(), [key(2), key(3), key(1)]);
        // 2 is now the oldest.
        cache.insert(key(4), &text(10));
        assert_eq!(cache.keys(), [key(3), key(1), key(4)]);
    }

    #[test]
    fn cache_respects_the_byte_cap() {
        let cost = text(100).cost();
        let mut cache = PreviewCache::new(8, cost * 2 + cost / 2);
        cache.insert(key(1), &text(100));
        cache.insert(key(2), &text(100));
        assert_eq!(cache.keys().len(), 2);
        // A third no longer fits beside both: the oldest goes.
        let evicted = cache.insert(key(3), &text(100));
        assert_eq!(evicted.len(), 1);
        assert_eq!(cache.keys(), [key(2), key(3)]);
        assert!(cache.bytes <= cache.max_bytes);
    }

    #[test]
    fn a_big_entry_pushes_out_several_small_ones() {
        let small = text(100).cost();
        let mut cache = PreviewCache::new(8, small * 4);
        for n in 1..=4 {
            cache.insert(key(n), &text(100));
        }
        // One about three times as big: needs three of the four gone.
        let evicted = cache.insert(key(5), &text(300));
        assert_eq!(evicted.len(), 3);
        assert_eq!(cache.keys(), [key(4), key(5)]);
    }

    #[test]
    fn an_entry_over_the_cap_is_not_kept_and_pushes_nothing_out() {
        let cost = text(100).cost();
        let mut cache = PreviewCache::new(8, cost * 2);
        cache.insert(key(1), &text(100));
        let evicted = cache.insert(key(2), &text(10_000));
        assert!(evicted.is_empty());
        assert_eq!(cache.keys(), [key(1)]);
        assert!(cache.get(&key(2)).is_none());
        assert_eq!(cache.bytes, cost);
    }

    #[test]
    fn inserting_a_key_again_replaces_it() {
        let mut cache = PreviewCache::new(8, u64::MAX);
        cache.insert(key(1), &text(10));
        cache.insert(key(2), &text(10));
        let evicted = cache.insert(key(1), &text(20));
        assert_eq!(evicted.len(), 1);
        assert_eq!(cache.keys(), [key(2), key(1)]);
        assert_eq!(cache.bytes, text(10).cost() + text(20).cost());
    }

    #[test]
    fn the_stream_is_part_of_the_key() {
        let mut cache = PreviewCache::new(8, u64::MAX);
        let data = key(1);
        let resource = (data.0, data.1, Stream::Resource);
        cache.insert(data, &text(10));
        assert!(cache.get(&resource).is_none());
        cache.insert(resource, &Preview::None);
        assert!(cache.get(&data).is_some());
        assert!(cache.get(&resource).is_some());
    }

    #[test]
    fn gate_discards_results_of_older_generations() {
        let mut gate = Gate::new();
        let (first, first_cancel) = gate.restart();
        assert!(gate.is_current(first));
        assert!(!first_cancel.load(Ordering::Relaxed));

        let (second, second_cancel) = gate.restart();
        assert!(
            first_cancel.load(Ordering::Relaxed),
            "the old load is cancelled"
        );
        assert!(!second_cancel.load(Ordering::Relaxed));
        assert!(!gate.is_current(first));
        assert!(gate.is_current(second));
        assert_eq!(gate.generation, second);
    }

    #[test]
    fn tabs_cycle_both_ways() {
        assert_eq!(Tab::Preview.cycle(true), Tab::Bytes);
        assert_eq!(Tab::Bytes.cycle(true), Tab::Details);
        assert_eq!(Tab::Details.cycle(true), Tab::Preview);
        assert_eq!(Tab::Preview.cycle(false), Tab::Details);
        assert_eq!(Tab::Details.cycle(false), Tab::Bytes);
        assert_eq!(Tab::Bytes.cycle(false), Tab::Preview);
    }

    #[test]
    fn a_resource_fork_opens_on_bytes() {
        assert_eq!(
            tab_after_fork_switch(Stream::Resource, Tab::Preview),
            Tab::Bytes
        );
        assert_eq!(
            tab_after_fork_switch(Stream::Resource, Tab::Details),
            Tab::Details
        );
        assert_eq!(
            tab_after_fork_switch(Stream::Data, Tab::Preview),
            Tab::Preview
        );
        assert_eq!(tab_after_fork_switch(Stream::Data, Tab::Bytes), Tab::Bytes);
    }

    #[test]
    fn the_fork_switch_needs_forks_on_the_volume_and_a_fork_with_bytes() {
        assert!(fork_switch_visible(true, Some(1)));
        assert!(!fork_switch_visible(true, Some(0)));
        assert!(!fork_switch_visible(true, None));
        assert!(!fork_switch_visible(false, Some(1_024)));
    }

    #[test]
    fn a_part_that_cannot_be_said_is_worded_for_the_user() {
        assert_eq!(
            part_error(&Error::Unsupported("extents".into())),
            "This volume does not say."
        );
        assert!(part_error(&Error::NotFound { node: 7 }).starts_with("Could not be read ("));
    }

    #[test]
    fn engine_errors_are_wrapped_and_provider_sentences_are_not() {
        let engine = anyhow::Error::from(Error::NotFound { node: 7 });
        assert!(message(&engine).starts_with("This file could not be read ("));
        let own = anyhow::anyhow!("This picture's header is damaged.");
        assert_eq!(message(&own), "This picture's header is damaged.");
    }
}
