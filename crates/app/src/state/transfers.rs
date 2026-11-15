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

//! Copy jobs to the host: the queue, the progress and the results (`m5-quicklook-copy.md` 6).
//!
//! [`Transfers`] holds a queue of jobs and runs one at a time. A job owns the `Arc<dyn
//! FileSystem>` it reads, so it outlives navigation. It runs `extract::copy_out` on the
//! background executor; the events go through a channel, and the entity empties the channel on a
//! 50 ms timer while a job runs and notifies at most once per drain, never from the callback
//! itself. Every message belongs to the job whose channel it came from, so a result for a job
//! that is gone cannot land on another one.
//!
//! The words (the footer's progress line, the notices, the details) and the arithmetic behind
//! them are plain functions above the entity, tested without a window.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use disk_atlas_lib::extract::{CopyEvent, CopyOptions, CopySummary, Item, copy_out};
use disk_atlas_lib::fs::{FileSystem, NodeId, NodeKind};
use gpui::{App, Context, PathPromptOptions, Task};

use super::listing::Listing;
use crate::format;

/// How often the entity empties the event channel while a job runs.
pub const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// The label of the folder prompt's confirm button.
const PROMPT_LABEL: &str = "Copy here";

/// What a failed job tells the user to do next.
const CHOOSE_ANOTHER: &str = "Choose another folder.";

// ---- words and numbers ---------------------------------------------------------------------

/// "1 item", "4 items": `format::items` for a count that is not a `usize`.
fn items_text(count: u64) -> String {
    format!(
        "{} {}",
        format::grouped(count),
        if count == 1 { "item" } else { "items" }
    )
}

/// `one` for 1, `many` for everything else, after the grouped count: "1 link", "2 links".
fn counted(count: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        format::grouped(count),
        if count == 1 { one } else { many }
    )
}

/// The footer's progress line while a job runs: "Copying 1,204 items \u{b7} 3.1 GB", plus
/// "\u{b7} 2 more waiting" when jobs are queued behind it. Before the first file starts there is
/// nothing to count, so it says that the copy is starting.
pub fn progress_text(items: u64, bytes: u64, waiting: usize) -> String {
    let mut line = if items == 0 && bytes == 0 {
        "Starting the copy\u{2026}".to_owned()
    } else {
        format!(
            "Copying {} \u{b7} {}",
            items_text(items),
            format::size(bytes)
        )
    };
    if waiting > 0 {
        line.push_str(&format!(
            " \u{b7} {} more waiting",
            format::grouped(waiting as u64)
        ));
    }
    line
}

/// How much of a job of known size is written: `0.0..=1.0`. `None` when the total is not known
/// (a folder is among the items) or is zero.
pub fn fraction(bytes: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|&total| total > 0)?;
    Some((bytes as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

/// The name of a folder for a sentence: its last part, or the whole path for a drive root.
pub fn destination_label(dest: &Path) -> String {
    dest.file_name().map_or_else(
        || dest.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The items and the byte total of a selection. The total is known only when every selected row
/// is a file whose size the listing gave: a folder's content is not counted until it is walked.
pub fn items_of<'a>(
    rows: impl IntoIterator<Item = (NodeId, &'a str, NodeKind, Option<u64>)>,
) -> (Vec<Item>, Option<u64>) {
    let mut items = Vec::new();
    let mut total = Some(0u64);
    for (node, name, kind, size) in rows {
        items.push(Item {
            node,
            name: name.to_owned(),
        });
        total = match (total, kind, size) {
            (Some(sum), NodeKind::File, Some(size)) => Some(sum.saturating_add(size)),
            _ => None,
        };
    }
    (items, total)
}

/// The things left out or not read, as the sentence fragments both the notice and the details
/// use: "4 could not be read", "2 links were not copied".
fn problem_parts(summary: &CopySummary) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = |count: u64, text: String| {
        if count > 0 {
            parts.push(text);
        }
    };
    let (failed, links, other, system) = (
        summary.failed,
        summary.skipped_links,
        summary.skipped_other,
        summary.skipped_system,
    );
    part(
        failed,
        format!("{} could not be read", format::grouped(failed)),
    );
    part(
        links,
        format!(
            "{} {} not copied",
            counted(links, "link", "links"),
            if links == 1 { "was" } else { "were" }
        ),
    );
    part(
        other,
        format!(
            "{} {} not copied",
            counted(other, "other item", "other items"),
            if other == 1 { "was" } else { "were" }
        ),
    );
    part(
        system,
        format!(
            "{} {} left behind",
            counted(system, "housekeeping file", "housekeeping files"),
            if system == 1 { "was" } else { "were" }
        ),
    );
    parts
}

/// What the details panel says: the counts and the first failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    /// "Copied 1,198 of 1,204 items to Downloads"
    pub heading: String,
    /// One line per kind of problem: "4 could not be read".
    pub lines: Vec<String>,
    /// The volume path and the error text of the first failures.
    pub failures: Vec<(String, String)>,
    /// Failures past the ones listed.
    pub more: u64,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// Everything was copied.
    Done,
    /// Some things were not copied; the notice has details.
    Problems,
    /// The user stopped the job.
    Stopped,
    /// The job could not run or could not go on (the destination, the host disk).
    Failed,
}

/// The footer's account of the last job, shown in place of the item count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The job it is about.
    pub job: JobId,
    /// How it ended.
    pub kind: NoticeKind,
    /// The sentence.
    pub text: String,
    /// The first path created in the destination, for "Show".
    pub reveal: Option<PathBuf>,
    /// The panel behind "Details", for a job with problems.
    pub details: Option<Details>,
}

/// The notice for a finished job. Housekeeping files left behind on purpose are mentioned but do
/// not make the copy a problem; failures, links and other skipped entries do.
pub fn notice_for(job: JobId, dest: &Path, summary: &CopySummary) -> Notice {
    let copied = summary.files.saturating_add(summary.folders);
    let reveal = summary.created.first().cloned();
    if summary.cancelled {
        return Notice {
            job,
            kind: NoticeKind::Stopped,
            text: format!(
                "Copy stopped. {} {} copied.",
                items_text(copied),
                if copied == 1 { "was" } else { "were" }
            ),
            reveal,
            details: None,
        };
    }
    let place = destination_label(dest);
    let parts = problem_parts(summary);
    let problems = summary
        .failed
        .saturating_add(summary.skipped_links)
        .saturating_add(summary.skipped_other)
        > 0;
    if !problems {
        let mut text = format!("Copied {} to {place}.", items_text(copied));
        if summary.skipped_system > 0 {
            text.push(' ');
            text.push_str(&capitalised(&parts.join(", ")));
            text.push('.');
        }
        return Notice {
            job,
            kind: NoticeKind::Done,
            text,
            reveal,
            details: None,
        };
    }
    let total = copied
        .saturating_add(summary.failed)
        .saturating_add(summary.skipped_links)
        .saturating_add(summary.skipped_other)
        .saturating_add(summary.skipped_system);
    let heading = format!(
        "Copied {} of {} to {place}",
        format::grouped(copied),
        items_text(total)
    );
    let listed = summary.first_failures.len() as u64;
    Notice {
        job,
        kind: NoticeKind::Problems,
        text: format!(
            "Copied {} of {}. {}.",
            format::grouped(copied),
            items_text(total),
            capitalised(&parts.join(", "))
        ),
        reveal,
        details: Some(Details {
            heading,
            lines: parts.iter().map(|part| capitalised(part)).collect(),
            failures: summary.first_failures.clone(),
            more: summary.failed.saturating_sub(listed),
        }),
    }
}

/// The notice for a job `copy_out` refused (the destination is not a writable folder, the host
/// disk is full): the engine's words, then what to do.
pub fn failure_notice(job: JobId, dest: &Path, error: &str) -> Notice {
    let error = error.trim_end_matches('.');
    Notice {
        job,
        kind: NoticeKind::Failed,
        text: format!(
            "Could not copy to {}: {error}. {CHOOSE_ANOTHER}",
            destination_label(dest)
        ),
        reveal: None,
        details: None,
    }
}

/// `text` with its first letter in upper case.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether the copy button and key can act now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// The source is the computer's own filesystem: there is nothing to copy out of (R8 copies
    /// there). No button, no menu item.
    NotHere,
    /// Nothing is selected. The button is greyed and says why.
    Nothing,
    /// Something is selected.
    Ready,
}

/// What the details pane and the key offer, from the source and the selection.
pub fn offer(on_host: bool, selected: usize) -> Offer {
    if on_host {
        Offer::NotHere
    } else if selected == 0 {
        Offer::Nothing
    } else {
        Offer::Ready
    }
}

/// The tooltip of the greyed copy button.
pub const NOTHING_SELECTED: &str = "Select the files or folders to copy first.";

/// The options a job runs with. `hidden` stays at the engine's default (everything the user can
/// see is copied, hidden items included) and so do the host's naming rules.
fn options_for(forks: bool, copy_system_items: bool) -> CopyOptions {
    CopyOptions {
        forks,
        copy_system_items,
        ..CopyOptions::default()
    }
}

// ---- the selection -------------------------------------------------------------------------

/// What the user selected, read from memory on the UI thread: everything a job needs to run on
/// its own.
#[derive(Clone)]
pub struct Selection {
    /// The filesystem the rows belong to.
    pub fs: Arc<dyn FileSystem>,
    /// The selected rows, in display order. In search results too: on a disk a hit's node is the
    /// volume's own.
    pub items: Vec<Item>,
    /// The total bytes when only files are selected.
    pub total_bytes: Option<u64>,
    /// The volume has resource forks (the fork question applies).
    pub resource_forks: bool,
}

impl Selection {
    /// The selection of `listing`, or `None` when the source is the computer's own filesystem or
    /// nothing is selected. Only reads memory.
    pub fn from_listing(listing: &Listing, cx: &App) -> Option<Self> {
        let location = listing.location()?;
        if listing.source_facts(cx)?.host {
            return None;
        }
        let rows = listing
            .selected_positions(usize::MAX)
            .into_iter()
            .filter_map(|position| listing.row(position));
        let (items, total_bytes) = items_of(rows.map(|row| {
            (
                row.entry.node,
                row.entry.name,
                row.entry.kind,
                row.entry.size,
            )
        }));
        if items.is_empty() {
            return None;
        }
        let fs = Arc::clone(location.fs());
        let resource_forks = fs.info().resource_forks;
        Some(Self {
            fs,
            items,
            total_bytes,
            resource_forks,
        })
    }
}

// ---- jobs ----------------------------------------------------------------------------------

/// Which job something belongs to. Never reused in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(pub u64);

/// What the footer counts while a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Files started.
    pub items: u64,
    /// Bytes written so far.
    pub bytes: u64,
    /// The job's byte total, when only files are copied.
    pub total_bytes: Option<u64>,
}

/// One copy, waiting or running.
struct Job {
    id: JobId,
    fs: Arc<dyn FileSystem>,
    items: Vec<Item>,
    dest: PathBuf,
    options: CopyOptions,
    cancel: Arc<AtomicBool>,
    progress: Progress,
}

/// What the copy thread sends back.
enum Msg {
    /// A file started.
    Item,
    /// Cumulative bytes written.
    Bytes(u64),
    /// The job is over: its summary, or the engine's words for why it could not run.
    Done(Result<CopySummary, String>),
}

/// The job being copied and the channel its events arrive on.
struct Running {
    job: Job,
    rx: Receiver<Msg>,
    /// The copy itself on the background executor. Held so that it lives as long as the job.
    task: Option<Task<()>>,
}

/// A job ready to run on a background thread. Owns everything it needs.
pub struct Launch {
    fs: Arc<dyn FileSystem>,
    items: Vec<Item>,
    dest: PathBuf,
    options: CopyOptions,
    cancel: Arc<AtomicBool>,
    tx: Sender<Msg>,
}

impl Launch {
    /// Copies, sending events to the entity. Blocking: run it on the background executor.
    pub fn run(self) {
        let tx = self.tx.clone();
        let mut on_event = |event: CopyEvent<'_>| match event {
            CopyEvent::Started { .. } => {
                tx.send(Msg::Item).ok();
            }
            CopyEvent::Bytes { total_bytes } => {
                tx.send(Msg::Bytes(total_bytes)).ok();
            }
            // Failures and skips are in the summary, with their text.
            _ => {}
        };
        let result = copy_out(
            self.fs.as_ref(),
            &self.items,
            &self.dest,
            &self.options,
            &self.cancel,
            &mut on_event,
        )
        .map_err(|error| error.to_string());
        self.tx.send(Msg::Done(result)).ok();
    }
}

/// What one drain of the channel changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drained {
    /// A counter moved or the job ended: the views repaint, once.
    pub changed: bool,
    /// The running job ended.
    pub finished: bool,
}

/// The fork question, asked once per session before the first copy from a volume that has
/// forks.
#[derive(Clone)]
pub struct Pending {
    /// The copy that waits for the answer.
    pub selection: Selection,
    /// The checkbox: write AppleDouble sidecars for the forks.
    pub keep_forks: bool,
}

/// The queue of copy jobs and the session's copy choices.
pub struct Transfers {
    waiting: VecDeque<Job>,
    running: Option<Running>,
    next_id: u64,
    notice: Option<Notice>,
    pending: Option<Pending>,
    /// The fork choice, once made. `None` until asked.
    keep_forks: Option<bool>,
    /// "Also copy housekeeping files". Session only.
    copy_system_items: bool,
    /// The folder prompt is open.
    choosing: bool,
    details_open: bool,
    /// The drain timer. Replaced, never dropped from inside itself.
    ticker: Option<Task<()>>,
    ticking: bool,
}

impl Default for Transfers {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Transfers {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

impl Transfers {
    /// An empty queue.
    pub fn new() -> Self {
        Self {
            waiting: VecDeque::new(),
            running: None,
            next_id: 1,
            notice: None,
            pending: None,
            keep_forks: None,
            copy_system_items: false,
            choosing: false,
            details_open: false,
            ticker: None,
            ticking: false,
        }
    }

    // ---- what the views read ---------------------------------------------------------------

    /// A job is running or waiting.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.waiting.is_empty()
    }

    /// The footer's progress line and bar while a job runs.
    pub fn running_view(&self) -> Option<(String, Option<f32>)> {
        let running = self.running.as_ref()?;
        let progress = running.job.progress;
        Some((
            progress_text(progress.items, progress.bytes, self.waiting.len()),
            fraction(progress.bytes, progress.total_bytes),
        ))
    }

    /// The account of the last job, until the next navigation or job.
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// The fork question, while it waits for an answer.
    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    /// The details panel is open.
    pub fn details_open(&self) -> bool {
        self.details_open
    }

    /// The session's fork choice (off until it is made).
    pub fn forks_on(&self) -> bool {
        self.keep_forks.unwrap_or(false)
    }

    /// "Also copy housekeeping files".
    pub fn system_items_on(&self) -> bool {
        self.copy_system_items
    }

    // ---- state changes that need no window -------------------------------------------------

    /// Adds a job to the queue and returns its id. A new job clears the last notice.
    pub fn push(&mut self, selection: Selection, dest: PathBuf) -> JobId {
        let id = JobId(self.next_id);
        self.next_id += 1;
        let forks = selection.resource_forks && self.forks_on();
        self.waiting.push_back(Job {
            id,
            fs: selection.fs,
            items: selection.items,
            dest,
            options: options_for(forks, self.copy_system_items),
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Progress {
                total_bytes: selection.total_bytes,
                ..Progress::default()
            },
        });
        id
    }

    /// Moves the next waiting job to running when none is, and hands back what to run. The notice
    /// of the job before goes.
    pub fn take_next(&mut self) -> Option<Launch> {
        if self.running.is_some() {
            return None;
        }
        let job = self.waiting.pop_front()?;
        self.notice = None;
        self.details_open = false;
        let (tx, rx) = mpsc::channel();
        let launch = Launch {
            fs: Arc::clone(&job.fs),
            items: job.items.clone(),
            dest: job.dest.clone(),
            options: job.options,
            cancel: Arc::clone(&job.cancel),
            tx,
        };
        self.running = Some(Running {
            job,
            rx,
            task: None,
        });
        Some(launch)
    }

    /// Reads everything the copy thread has sent. When it sent the end, the job becomes the
    /// notice and the next one may start.
    pub fn drain_messages(&mut self) -> Drained {
        let Some(running) = self.running.as_mut() else {
            return Drained::default();
        };
        let mut changed = false;
        let mut end: Option<Result<CopySummary, String>> = None;
        loop {
            match running.rx.try_recv() {
                Ok(Msg::Item) => {
                    running.job.progress.items = running.job.progress.items.saturating_add(1);
                    changed = true;
                }
                Ok(Msg::Bytes(bytes)) => {
                    running.job.progress.bytes = bytes;
                    changed = true;
                }
                Ok(Msg::Done(result)) => {
                    end = Some(result);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                // The thread died without a word (a panic): the job is over.
                Err(TryRecvError::Disconnected) => {
                    end = Some(Err("the copy stopped unexpectedly".to_owned()));
                    break;
                }
            }
        }
        let Some(result) = end else {
            return Drained {
                changed,
                finished: false,
            };
        };
        if let Some(Running { job, .. }) = self.running.take() {
            self.notice = Some(match result {
                Ok(summary) => notice_for(job.id, &job.dest, &summary),
                Err(error) => failure_notice(job.id, &job.dest, &error),
            });
        }
        Drained {
            changed: true,
            finished: true,
        }
    }

    /// Stops the running job. It ends with a "Copy stopped" notice; the next one waiting starts.
    pub fn cancel_running(&mut self) {
        if let Some(running) = &self.running {
            running.job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Stops the running job and forgets the waiting ones (the app is quitting).
    pub fn cancel_all(&mut self) {
        for job in self.waiting.drain(..) {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.cancel_running();
    }

    /// Hides the notice. True when there was one.
    pub fn dismiss_notice_now(&mut self) -> bool {
        self.details_open = false;
        self.notice.take().is_some()
    }

    // ---- entry points that need the app ----------------------------------------------------

    /// "Copy to this computer": asks the fork question first when it applies, then for the
    /// folder, then queues the job.
    pub fn request(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if self.choosing {
            return;
        }
        if selection.resource_forks && self.keep_forks.is_none() {
            self.pending = Some(Pending {
                selection,
                keep_forks: false,
            });
            cx.notify();
            return;
        }
        self.choose_folder(selection, cx);
    }

    /// The checkbox of the fork strip.
    pub fn set_pending_forks(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(pending) = &mut self.pending
            && pending.keep_forks != on
        {
            pending.keep_forks = on;
            cx.notify();
        }
    }

    /// The fork strip's "Copy": remembers the answer for the session and asks for the folder.
    pub fn confirm_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.take() {
            self.keep_forks = Some(pending.keep_forks);
            cx.notify();
            self.choose_folder(pending.selection, cx);
        }
    }

    /// The fork strip's "Cancel".
    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending.take().is_some() {
            cx.notify();
        }
    }

    /// The copy menu's fork switch.
    pub fn toggle_forks(&mut self, cx: &mut Context<Self>) {
        self.keep_forks = Some(!self.forks_on());
        cx.notify();
    }

    /// The copy menu's "Also copy housekeeping files".
    pub fn toggle_system_items(&mut self, cx: &mut Context<Self>) {
        self.copy_system_items = !self.copy_system_items;
        cx.notify();
    }

    /// Shows the details of a notice with problems, or hides them.
    pub fn set_details_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let open = open
            && self
                .notice
                .as_ref()
                .is_some_and(|notice| notice.details.is_some());
        if self.details_open != open {
            self.details_open = open;
            cx.notify();
        }
    }

    /// The notice's close button, and a navigation.
    pub fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        if self.dismiss_notice_now() {
            cx.notify();
        }
    }

    /// The footer's "Cancel".
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel_running();
        cx.notify();
    }

    /// Shows the platform's folder prompt, then queues the job. The prompt takes no starting
    /// folder in this GPUI, so the user starts where the system puts them.
    fn choose_folder(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.choosing = true;
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(PROMPT_LABEL.into()),
        });
        cx.spawn(async move |this, cx| {
            let answer = chosen.await;
            this.update(cx, |this, cx| {
                this.choosing = false;
                match answer {
                    Ok(Ok(Some(paths))) => {
                        if let Some(dest) = paths.into_iter().next() {
                            this.enqueue(selection, dest, cx);
                        }
                    }
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "the folder prompt failed");
                        this.notice = Some(Notice {
                            job: JobId(0),
                            kind: NoticeKind::Failed,
                            text: "The folder chooser could not open. Try again.".to_owned(),
                            reveal: None,
                            details: None,
                        });
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Queues a copy into `dest` and starts it when nothing runs.
    pub fn enqueue(&mut self, selection: Selection, dest: PathBuf, cx: &mut Context<Self>) {
        self.push(selection, dest);
        self.start_next(cx);
        cx.notify();
    }

    /// Starts the next waiting job on the background executor and the drain timer if it is not
    /// ticking.
    fn start_next(&mut self, cx: &mut Context<Self>) {
        let Some(launch) = self.take_next() else {
            return;
        };
        let task = cx.background_executor().spawn(async move { launch.run() });
        if let Some(running) = &mut self.running {
            running.task = Some(task);
        }
        if !self.ticking {
            self.ticking = true;
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(DRAIN_INTERVAL).await;
                    let again = this.update(cx, |this, cx| this.drain(cx)).unwrap_or(false);
                    if !again {
                        break;
                    }
                }
            }));
        }
    }

    /// One tick: empties the channel, notifies once if anything changed, and starts the next job
    /// after one that ended. False when there is nothing left to wait for.
    fn drain(&mut self, cx: &mut Context<Self>) -> bool {
        let drained = self.drain_messages();
        if drained.finished {
            self.start_next(cx);
        }
        if drained.changed {
            cx.notify();
        }
        let again = self.busy();
        if !again {
            self.ticking = false;
        }
        again
    }
}

#[cfg(test)]
mod tests {
    use disk_atlas_lib::fs::native::NativeFs;
    use gpui::AppContext as _;

    use super::*;

    // ---- the words -----------------------------------------------------------------------

    #[test]
    fn the_progress_line_counts_items_and_bytes() {
        assert_eq!(progress_text(0, 0, 0), "Starting the copy\u{2026}");
        assert_eq!(
            progress_text(1_204, 3_328_599_654, 0),
            "Copying 1,204 items \u{b7} 3.1 GB"
        );
        assert_eq!(progress_text(1, 512, 0), "Copying 1 item \u{b7} 512 bytes");
    }

    #[test]
    fn queued_jobs_are_counted_in_the_progress_line() {
        assert_eq!(
            progress_text(3, 2_048, 2),
            "Copying 3 items \u{b7} 2 KB \u{b7} 2 more waiting"
        );
        assert!(progress_text(0, 0, 1).ends_with("1 more waiting"));
    }

    #[test]
    fn the_bar_needs_a_known_total() {
        assert_eq!(fraction(50, Some(200)), Some(0.25));
        assert_eq!(fraction(0, Some(200)), Some(0.0));
        // Bytes of resource forks can pass the total of the data forks: the bar stops at full.
        assert_eq!(fraction(300, Some(200)), Some(1.0));
        assert_eq!(fraction(10, None), None);
        assert_eq!(fraction(10, Some(0)), None);
    }

    #[test]
    fn a_destination_is_named_by_its_last_part() {
        assert_eq!(
            destination_label(Path::new("/home/me/Downloads")),
            "Downloads"
        );
        assert_eq!(destination_label(Path::new("/")), "/");
    }

    #[test]
    fn files_have_a_total_and_a_folder_takes_it_away() {
        let file = |n: u64, size| (NodeId(n), "f", NodeKind::File, size);
        let (items, total) = items_of([file(1, Some(10)), file(2, Some(5))]);
        assert_eq!(items.len(), 2);
        assert_eq!(total, Some(15));
        let (_, total) = items_of([
            file(1, Some(10)),
            (NodeId(3), "d", NodeKind::Directory, None),
        ]);
        assert_eq!(total, None);
        // A file whose size the listing does not know: no total either.
        let (_, total) = items_of([file(1, None)]);
        assert_eq!(total, None);
        // Once unknown, it stays unknown.
        let (_, total) = items_of([
            (NodeId(3), "d", NodeKind::Directory, None),
            file(1, Some(10)),
        ]);
        assert_eq!(total, None);
        let (items, total) = items_of(std::iter::empty());
        assert!(items.is_empty());
        assert_eq!(total, Some(0));
    }

    fn summary(files: u64, folders: u64) -> CopySummary {
        // `CopySummary` is `#[non_exhaustive]`: another crate cannot build it with `..`.
        let mut summary = CopySummary::default();
        summary.files = files;
        summary.folders = folders;
        summary.created = vec![PathBuf::from("/dest/First")];
        summary
    }

    #[test]
    fn a_clean_copy_says_how_many_and_where() {
        let notice = notice_for(
            JobId(1),
            Path::new("/home/me/Downloads"),
            &summary(1_000, 204),
        );
        assert_eq!(notice.kind, NoticeKind::Done);
        assert_eq!(notice.text, "Copied 1,204 items to Downloads.");
        assert_eq!(notice.reveal, Some(PathBuf::from("/dest/First")));
        assert!(notice.details.is_none());
        let one = notice_for(JobId(1), Path::new("/d/Here"), &summary(1, 0));
        assert_eq!(one.text, "Copied 1 item to Here.");
    }

    #[test]
    fn problems_are_counted_and_carry_details() {
        let mut result = summary(1_100, 98);
        result.failed = 4;
        result.skipped_links = 2;
        result.first_failures = vec![
            ("Docs/a".to_owned(), "read error".to_owned()),
            ("Docs/b".to_owned(), "read error".to_owned()),
        ];
        let notice = notice_for(JobId(2), Path::new("/home/me/Downloads"), &result);
        assert_eq!(notice.kind, NoticeKind::Problems);
        assert_eq!(
            notice.text,
            "Copied 1,198 of 1,204 items. 4 could not be read, 2 links were not copied."
        );
        let details = notice.details.expect("a notice with problems has details");
        assert_eq!(details.heading, "Copied 1,198 of 1,204 items to Downloads");
        assert_eq!(
            details.lines,
            ["4 could not be read", "2 links were not copied"]
        );
        assert_eq!(details.failures.len(), 2);
        // Four failed, two listed: two more.
        assert_eq!(details.more, 2);
    }

    #[test]
    fn single_problems_use_the_singular() {
        let mut result = summary(9, 0);
        result.skipped_links = 1;
        result.skipped_other = 1;
        result.failed = 1;
        let notice = notice_for(JobId(1), Path::new("/d/X"), &result);
        assert_eq!(
            notice.text,
            "Copied 9 of 12 items. 1 could not be read, 1 link was not copied, 1 other item was not copied."
        );
    }

    #[test]
    fn housekeeping_files_left_behind_are_said_but_are_not_a_problem() {
        let mut result = summary(10, 2);
        result.skipped_system = 3;
        let notice = notice_for(JobId(1), Path::new("/d/Out"), &result);
        assert_eq!(notice.kind, NoticeKind::Done);
        assert_eq!(
            notice.text,
            "Copied 12 items to Out. 3 housekeeping files were left behind."
        );
        assert!(notice.details.is_none());
        // Next to a real problem they are counted in the total and in the details.
        result.failed = 1;
        let notice = notice_for(JobId(1), Path::new("/d/Out"), &result);
        assert_eq!(notice.kind, NoticeKind::Problems);
        assert!(
            notice
                .text
                .starts_with("Copied 12 of 16 items. 1 could not be read, 3 housekeeping")
        );
    }

    #[test]
    fn a_stopped_copy_says_what_was_copied() {
        let mut result = summary(300, 12);
        result.cancelled = true;
        let notice = notice_for(JobId(3), Path::new("/d/Out"), &result);
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert_eq!(notice.text, "Copy stopped. 312 items were copied.");
        result.files = 1;
        result.folders = 0;
        assert_eq!(
            notice_for(JobId(3), Path::new("/d/Out"), &result).text,
            "Copy stopped. 1 item was copied."
        );
    }

    #[test]
    fn a_refused_job_says_what_to_do_next() {
        let notice = failure_notice(JobId(4), Path::new("/d/Full"), "the disk is full.");
        assert_eq!(notice.kind, NoticeKind::Failed);
        assert_eq!(
            notice.text,
            "Could not copy to Full: the disk is full. Choose another folder."
        );
        assert!(notice.reveal.is_none());
    }

    #[test]
    fn the_offer_follows_the_source_and_the_selection() {
        assert_eq!(offer(true, 3), Offer::NotHere);
        assert_eq!(offer(true, 0), Offer::NotHere);
        assert_eq!(offer(false, 0), Offer::Nothing);
        assert_eq!(offer(false, 1), Offer::Ready);
    }

    // ---- the queue -----------------------------------------------------------------------

    /// A fresh folder under the system temp folder, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "disk-atlas-transfers-test-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the folder");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A source folder with `a.txt` and `b.txt` and a selection of both.
    fn source(label: &str) -> (TempDir, Selection) {
        let dir = TempDir::new(label);
        std::fs::write(dir.0.join("a.txt"), b"alpha").unwrap();
        std::fs::write(dir.0.join("b.txt"), b"bravo!").unwrap();
        let fs = NativeFs::new(dir.0.clone(), "Source").expect("folder opens");
        let items = ["a.txt", "b.txt"]
            .into_iter()
            .map(|name| Item {
                node: fs.lookup(fs.root(), name).unwrap().expect("exists"),
                name: name.to_owned(),
            })
            .collect();
        let selection = Selection {
            fs: Arc::new(fs),
            items,
            total_bytes: Some(11),
            resource_forks: false,
        };
        (dir, selection)
    }

    /// Runs a launched job to its end on this thread and drains what it sent.
    fn run_to_end(transfers: &mut Transfers, launch: Launch) -> Drained {
        launch.run();
        transfers.drain_messages()
    }

    #[test]
    fn jobs_run_one_at_a_time_in_the_order_they_were_queued() {
        let (_source, selection) = source("order");
        let (dest_a, dest_b) = (TempDir::new("order-a"), TempDir::new("order-b"));
        let mut transfers = Transfers::new();
        let first = transfers.push(selection.clone(), dest_a.0.clone());
        let second = transfers.push(selection, dest_b.0.clone());
        assert_ne!(first, second);
        assert!(transfers.busy());

        let launch = transfers.take_next().expect("the first job starts");
        // One at a time: nothing else starts while it runs.
        assert!(transfers.take_next().is_none());
        let view = transfers.running_view().expect("a job runs");
        assert!(view.0.ends_with("1 more waiting"), "{}", view.0);

        let drained = run_to_end(&mut transfers, launch);
        assert!(drained.finished && drained.changed);
        let notice = transfers.notice().expect("the first job left a notice");
        assert_eq!(notice.job, first);
        assert_eq!(notice.kind, NoticeKind::Done);
        assert!(dest_a.0.join("a.txt").exists() && dest_a.0.join("b.txt").exists());
        assert!(
            !dest_b.0.join("a.txt").exists(),
            "the second has not started"
        );

        // The second starts, and its start clears the first one's notice.
        let launch = transfers.take_next().expect("the second job starts");
        assert!(transfers.notice().is_none());
        run_to_end(&mut transfers, launch);
        assert_eq!(transfers.notice().map(|notice| notice.job), Some(second));
        assert!(dest_b.0.join("a.txt").exists());
        assert!(!transfers.busy());
        assert!(transfers.take_next().is_none());
    }

    #[test]
    fn cancelling_marks_the_job_stopped() {
        let (_source, selection) = source("cancel");
        let dest = TempDir::new("cancel-dest");
        let mut transfers = Transfers::new();
        transfers.push(selection, dest.0.clone());
        let launch = transfers.take_next().expect("starts");
        transfers.cancel_running();
        run_to_end(&mut transfers, launch);
        let notice = transfers.notice().expect("a notice");
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert!(notice.text.starts_with("Copy stopped."), "{}", notice.text);
        assert!(!transfers.busy());
    }

    #[test]
    fn a_destination_that_is_not_a_folder_fails_the_job_with_what_to_do() {
        let (_source, selection) = source("fail");
        let dir = TempDir::new("fail-dest");
        let missing = dir.0.join("not-there");
        let mut transfers = Transfers::new();
        transfers.push(selection, missing);
        let launch = transfers.take_next().expect("starts");
        run_to_end(&mut transfers, launch);
        let notice = transfers.notice().expect("a notice");
        assert_eq!(notice.kind, NoticeKind::Failed);
        assert!(
            notice.text.ends_with("Choose another folder."),
            "{}",
            notice.text
        );
    }

    #[test]
    fn one_drain_takes_a_whole_batch_and_reports_one_change() {
        let (_source, selection) = source("batch");
        let dest = TempDir::new("batch-dest");
        let mut transfers = Transfers::new();
        transfers.push(selection, dest.0.clone());
        let launch = transfers.take_next().expect("starts");
        // Two events are in the channel before the entity looks.
        launch.tx.send(Msg::Item).unwrap();
        launch.tx.send(Msg::Item).unwrap();
        launch.tx.send(Msg::Bytes(6)).unwrap();
        let drained = transfers.drain_messages();
        assert_eq!(
            drained,
            Drained {
                changed: true,
                finished: false
            }
        );
        let progress = transfers
            .running
            .as_ref()
            .expect("still running")
            .job
            .progress;
        assert_eq!((progress.items, progress.bytes), (2, 6));
        // Nothing new: nothing to repaint.
        assert_eq!(transfers.drain_messages(), Drained::default());
        // A thread that died without its end message still ends the job.
        drop(launch);
        let drained = transfers.drain_messages();
        assert!(drained.finished);
        assert_eq!(
            transfers.notice().map(|notice| notice.kind),
            Some(NoticeKind::Failed)
        );
    }

    #[test]
    fn options_follow_the_session_choices_and_the_volume() {
        let (_source, mut selection) = source("options");
        let dest = TempDir::new("options-dest");
        let mut transfers = Transfers::new();
        transfers.keep_forks = Some(true);
        transfers.copy_system_items = true;
        // A volume with no forks never asks for sidecars, whatever the switch says.
        transfers.push(selection.clone(), dest.0.clone());
        assert!(!transfers.waiting[0].options.forks);
        assert!(transfers.waiting[0].options.hidden);
        selection.resource_forks = true;
        transfers.push(selection, dest.0.clone());
        assert!(transfers.waiting[1].options.forks);
        assert!(transfers.waiting[0].options.copy_system_items);
    }

    #[test]
    fn dismissing_clears_the_notice_and_the_panel() {
        let mut transfers = Transfers::new();
        assert!(!transfers.dismiss_notice_now());
        transfers.notice = Some(failure_notice(JobId(1), Path::new("/x"), "no"));
        transfers.details_open = true;
        assert!(transfers.dismiss_notice_now());
        assert!(transfers.notice().is_none());
        assert!(!transfers.details_open());
    }

    // ---- with the app -----------------------------------------------------------------------

    #[gpui::test]
    fn a_queued_job_copies_on_the_background_executor(cx: &mut gpui::TestAppContext) {
        let (_source, selection) = source("app");
        let dest = TempDir::new("app-dest");
        let transfers = cx.update(|cx| cx.new(|_| Transfers::new()));
        let dest_path = dest.0.clone();
        transfers.update(cx, |transfers, cx| {
            transfers.enqueue(selection, dest_path, cx);
            assert!(transfers.busy());
        });
        for _ in 0..40 {
            cx.executor().advance_clock(DRAIN_INTERVAL * 2);
            cx.run_until_parked();
            if !transfers.read_with(cx, |transfers, _| transfers.busy()) {
                break;
            }
        }
        let notice = transfers.read_with(cx, |transfers, _| transfers.notice().cloned());
        assert_eq!(notice.map(|notice| notice.kind), Some(NoticeKind::Done));
        assert_eq!(std::fs::read(dest.0.join("b.txt")).unwrap(), b"bravo!");
    }
}
