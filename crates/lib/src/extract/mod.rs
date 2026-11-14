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

//! Copying files out to the host, with forks and metadata. Built in M5.
//!
//! [`copy_out`] copies selected files and folders of any [`FileSystem`] into
//! an existing host folder. Names from the volume go through
//! [`names::sanitize`] and [`names::NameSet::unique`] before they touch a
//! host path, nothing on the host is ever overwritten, and a file that
//! cannot be read whole leaves nothing behind. With
//! [`CopyOptions::forks`], resource forks and Finder information go into
//! AppleDouble `._name` sidecars ([`appledouble`]). Other extended
//! attributes are not written anywhere.
//!
//! The only things written are files and folders under the destination.

pub mod appledouble;
pub mod names;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self as host, File, FileTimes, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{CorruptKind, Corruption, Error, IoContext, Location, Result};
use crate::fs::{
    DirBatch, DirEntry, DirProgress, EntryFlags, FileSystem, MAX_EMPTY_BATCHES, Metadata, NodeId,
    NodeKind, Stream, Timestamp,
};
use crate::io::block::BlockSource;

use names::{NameSet, sanitize};

/// Deepest folder level copied; the top-level folder is level 1. Deeper
/// folders are skipped with [`Skip::TooDeep`].
pub const MAX_DEPTH: usize = 128;

/// Most times one folder is entered in a job. A folder reached by several
/// directory hard links is copied once per route (that is what the volume
/// shows), but a hostile volume can stack such links into a graph with
/// exponentially many routes; this bounds the work to this many times the
/// number of distinct folders.
pub const MAX_FOLDER_VISITS: u32 = 16;

/// Bytes read and written per step: the size of the one copy buffer.
pub const BUFFER_SIZE: usize = 1 << 20;

/// Most failures kept in [`CopySummary::first_failures`].
pub const MAX_FIRST_FAILURES: usize = 100;

/// Most names tried for one file or folder when the host keeps answering
/// that the name exists (something else is writing to the destination).
pub const MAX_NAME_TRIES: u32 = 100;

/// Entries asked of a folder listing per call.
const LIST_BATCH: usize = 1024;

/// Bytes between two [`CopyEvent::Bytes`].
const MIB: u64 = 1 << 20;

/// `FILE_FLAG_BACKUP_SEMANTICS`: lets `CreateFileW` open a directory.
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// `FILE_WRITE_ATTRIBUTES`: the access right `SetFileTime` needs.
#[cfg(windows)]
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;

/// A selected top-level entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The entry's node.
    pub node: NodeId,
    /// The entry's name on the volume. Sanitized like any other name.
    pub name: String,
}

/// Which host's naming rules names are made legal for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostRules {
    /// Windows: no `< > : " / \ | ? *` or control characters, no trailing
    /// dot or space, no device names, 255 UTF-16 units.
    Windows,
    /// Linux and other Unix: no `/` or NUL, 255 bytes.
    Unix,
    /// macOS: no NUL, `/` stored as `:`, 255 UTF-16 units.
    MacOs,
}

impl HostRules {
    /// The rules of the host this runs on.
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// What [`copy_out`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOptions {
    /// Write AppleDouble sidecars for resource forks and Finder information.
    pub forks: bool,
    /// Copy entries flagged [`EntryFlags::HIDDEN`]. Selected top-level
    /// items are always copied.
    pub hidden: bool,
    /// The naming rules. The host's own rules are applied on top, so a name
    /// is always legal where it is written.
    pub rules: HostRules,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            forks: false,
            hidden: true,
            rules: HostRules::current(),
        }
    }
}

/// Why an entry was left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Skip {
    /// A symbolic link. Links are never followed or recreated.
    Symlink,
    /// A device, socket or pipe.
    Special,
    /// A resource fork too long for an AppleDouble sidecar; the sidecar is
    /// left out, the file is copied.
    ForkTooLarge,
    /// A folder deeper than [`MAX_DEPTH`].
    TooDeep,
    /// An entry flagged hidden, with [`CopyOptions::hidden`] off.
    Hidden,
}

/// Progress reported by [`copy_out`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum CopyEvent<'a> {
    /// A file is about to be copied: its name on the volume and the size
    /// its listing gave.
    Started {
        /// The file's name on the volume.
        name: &'a str,
        /// Its size, if the listing knew it.
        size: Option<u64>,
    },
    /// Bytes written so far in the whole job (data and resource forks). At
    /// most once per MiB, plus once at the end of a file.
    Bytes {
        /// Cumulative bytes written.
        total_bytes: u64,
    },
    /// The job is over (also after a cancel). Not sent when `copy_out`
    /// fails.
    Finished {
        /// Files copied.
        total_files: u64,
    },
    /// One file or folder could not be copied; the copy goes on.
    Failed {
        /// Its path on the volume, from the top-level item's name.
        path: &'a str,
        /// What went wrong.
        error: &'a Error,
    },
    /// One entry was left out on purpose.
    Skipped {
        /// Its path on the volume, from the top-level item's name.
        path: &'a str,
        /// Why.
        why: Skip,
    },
}

/// What a [`copy_out`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CopySummary {
    /// Files copied whole.
    pub files: u64,
    /// Folders created.
    pub folders: u64,
    /// Bytes written: data forks and resource forks, not sidecar headers.
    pub bytes: u64,
    /// Files and folders that failed (also past the first 100).
    pub failed: u64,
    /// Symbolic links left out.
    pub skipped_links: u64,
    /// Everything else left out: special files, hidden entries, folders too
    /// deep, sidecars whose fork is too long.
    pub skipped_other: u64,
    /// Names changed to be legal or unique on the host.
    pub renamed: u64,
    /// The job was cancelled.
    pub cancelled: bool,
    /// Up to [`MAX_FIRST_FAILURES`]: the volume path and the error text.
    pub first_failures: Vec<(String, String)>,
    /// The paths written directly in the destination folder, sidecars
    /// included.
    pub created: Vec<PathBuf>,
}

/// Copies `items` of `fs` into the existing host folder `dest`.
///
/// Folders are copied with everything in them, files whole or not at all,
/// and nothing on the host is overwritten: a name already in use becomes
/// `name (2)`. Symbolic links and special files are skipped. A file or
/// folder that cannot be read is reported through `events` and counted, and
/// the copy goes on. `cancel` is checked before each entry and after each
/// MiB written; a cancelled job removes the file it was writing and returns
/// `Ok` with [`CopySummary::cancelled`] set.
///
/// `Err` is only for what ends the whole job: `dest` is not a writable
/// folder, it vanished, or the host disk is full.
pub fn copy_out(
    fs: &dyn FileSystem,
    items: &[Item],
    dest: &Path,
    options: &CopyOptions,
    cancel: &AtomicBool,
    events: &mut dyn FnMut(CopyEvent<'_>),
) -> Result<CopySummary> {
    let dest = host::canonicalize(dest).map_err(|e| Error::io(path_context("opening", dest), e))?;
    let meta = host::metadata(&dest).map_err(|e| Error::io(path_context("opening", &dest), e))?;
    if !meta.is_dir() {
        return Err(Error::io(
            path_context("opening", &dest),
            io::Error::from(io::ErrorKind::NotADirectory),
        ));
    }
    let mut names = NameSet::seeded_from_dir(&dest)
        .map_err(|e| Error::io(path_context("listing", &dest), e))?;

    let mut job = Job {
        fs,
        options,
        cancel,
        events,
        dest,
        buf: vec![0u8; BUFFER_SIZE],
        summary: CopySummary::default(),
        reported: 0,
        visits: HashMap::new(),
    };
    for item in items {
        let flow = job.check_cancel().and_then(|()| job.item(item, &mut names));
        match flow {
            Ok(()) => {}
            Err(Stop::Cancelled) => break,
            Err(Stop::Fatal(error)) => return Err(error),
        }
    }
    let total_files = job.summary.files;
    (job.events)(CopyEvent::Finished { total_files });
    Ok(job.summary)
}

/// Why the job stops early.
enum Stop {
    /// `cancel` was set.
    Cancelled,
    /// A failure that ends the whole job.
    Fatal(Error),
}

/// `Ok` to go on.
type Flow = std::result::Result<(), Stop>;

/// Why streaming one source to a host file stopped.
enum StreamError {
    /// The volume could not be read.
    Read(Error),
    /// The host refused the write.
    Write(io::Error),
    /// `cancel` was set.
    Cancelled,
}

/// What a file's listing (or metadata) says about it.
#[derive(Clone, Copy)]
struct FileFacts<'a> {
    node: NodeId,
    name: &'a str,
    size: Option<u64>,
    modified: Option<Timestamp>,
}

impl<'a> From<DirEntry<'a>> for FileFacts<'a> {
    fn from(entry: DirEntry<'a>) -> Self {
        Self {
            node: entry.node,
            name: entry.name,
            size: entry.size,
            modified: entry.modified,
        }
    }
}

/// A folder to copy or to finish, on the walk's explicit stack.
enum Frame {
    /// A folder already created on the host, to list and fill.
    Enter {
        node: NodeId,
        host: PathBuf,
        /// Its path on the volume, from the top-level item.
        vol: String,
        /// 1 for the top-level folder.
        depth: usize,
        modified: Option<Timestamp>,
        created: Option<Timestamp>,
    },
    /// A folder whose content is all written: leave it and set its times.
    Leave {
        node: NodeId,
        host: PathBuf,
        modified: Option<Timestamp>,
        created: Option<Timestamp>,
    },
}

/// One `copy_out` call.
struct Job<'a> {
    fs: &'a dyn FileSystem,
    options: &'a CopyOptions,
    cancel: &'a AtomicBool,
    events: &'a mut dyn FnMut(CopyEvent<'_>),
    /// The canonical destination.
    dest: PathBuf,
    /// The one copy buffer, [`BUFFER_SIZE`] bytes.
    buf: Vec<u8>,
    summary: CopySummary,
    /// `summary.bytes` at the last [`CopyEvent::Bytes`].
    reported: u64,
    /// How many times each folder was entered in this job, for
    /// [`MAX_FOLDER_VISITS`].
    visits: HashMap<NodeId, u32>,
}

impl Job<'_> {
    /// One selected item, copied into the destination.
    fn item(&mut self, item: &Item, names: &mut NameSet) -> Flow {
        let meta = match self.fs.metadata(item.node) {
            Ok(meta) => meta,
            Err(error) => {
                self.fail(&item.name, &error);
                return Ok(());
            }
        };
        let dest = self.dest.clone();
        match meta.kind {
            NodeKind::File => {
                let facts = FileFacts {
                    node: item.node,
                    name: &item.name,
                    size: meta.size,
                    modified: meta.modified,
                };
                self.file(&dest, names, &item.name, facts, Some(meta), true)
            }
            NodeKind::Directory => {
                let legal = self.host_name(&item.name);
                let Some((path, chosen, ())) =
                    self.create_unique(&dest, names, &legal, &item.name, true, |p| {
                        host::create_dir(p)
                    })?
                else {
                    return Ok(());
                };
                self.folder_made(&item.name, &chosen);
                self.summary.created.push(path.clone());
                if self.options.forks {
                    self.sidecar(
                        &dest,
                        names,
                        &chosen,
                        item.node,
                        meta.finder_info,
                        false,
                        &item.name,
                        true,
                    )?;
                }
                self.walk(Frame::Enter {
                    node: item.node,
                    host: path,
                    vol: item.name.clone(),
                    depth: 1,
                    modified: meta.modified,
                    created: meta.created,
                })
            }
            NodeKind::Symlink => {
                self.skip(&item.name, Skip::Symlink);
                Ok(())
            }
            NodeKind::Other => {
                self.skip(&item.name, Skip::Special);
                Ok(())
            }
        }
    }

    /// Copies a folder tree depth first from `first`, an `Enter` frame for
    /// a folder that already exists on the host.
    fn walk(&mut self, first: Frame) -> Flow {
        let mut stack = vec![first];
        // Folders from the top-level item down to the one being filled.
        let mut on_path: HashSet<NodeId> = HashSet::new();
        let mut batch = DirBatch::new();
        let mut vol = String::new();
        let mut children: Vec<Frame> = Vec::new();

        while let Some(frame) = stack.pop() {
            let (node, folder_host, folder_vol, depth, modified, created) = match frame {
                Frame::Leave {
                    node,
                    host,
                    modified,
                    created,
                } => {
                    on_path.remove(&node);
                    set_dir_times(&host, modified, created);
                    continue;
                }
                Frame::Enter {
                    node,
                    host,
                    vol,
                    depth,
                    modified,
                    created,
                } => (node, host, vol, depth, modified, created),
            };
            on_path.insert(node);
            batch.clear();
            self.list(node, &folder_vol, &mut batch);
            let mut names = NameSet::new();

            // Files first.
            for entry in batch.iter() {
                self.check_cancel()?;
                volume_path(&mut vol, &folder_vol, entry.name);
                if !self.wanted(&entry, &vol) {
                    continue;
                }
                match entry.kind {
                    NodeKind::File => {
                        self.file(&folder_host, &mut names, &vol, entry.into(), None, false)?;
                    }
                    NodeKind::Directory => {}
                    NodeKind::Symlink => self.skip(&vol, Skip::Symlink),
                    NodeKind::Other => self.skip(&vol, Skip::Special),
                }
            }

            // Then the subfolders: created now, filled after this folder.
            children.clear();
            for entry in batch.iter() {
                if entry.kind != NodeKind::Directory || !self.hidden_ok(&entry) {
                    continue;
                }
                self.check_cancel()?;
                volume_path(&mut vol, &folder_vol, entry.name);
                if depth >= MAX_DEPTH {
                    self.skip(&vol, Skip::TooDeep);
                    continue;
                }
                if on_path.contains(&entry.node) {
                    let error = Error::from(Corruption::new(
                        "folder tree",
                        Location::Object(entry.node.0),
                        CorruptKind::Loop {
                            what: "a folder containing itself",
                        },
                    ));
                    self.fail(&vol, &error);
                    continue;
                }
                let visits = self.visits.entry(entry.node).or_insert(0);
                if *visits >= MAX_FOLDER_VISITS {
                    let error = Error::from(Corruption::new(
                        "folder tree",
                        Location::Object(entry.node.0),
                        CorruptKind::Loop {
                            what: "a folder reached through too many routes",
                        },
                    ));
                    self.fail(&vol, &error);
                    continue;
                }
                *visits = visits.saturating_add(1);
                let legal = self.host_name(entry.name);
                let Some((path, chosen, ())) =
                    self.create_unique(&folder_host, &mut names, &legal, &vol, false, |p| {
                        host::create_dir(p)
                    })?
                else {
                    continue;
                };
                self.folder_made(entry.name, &chosen);
                let mut times = (entry.modified, None);
                if self.options.forks {
                    match self.fs.metadata(entry.node) {
                        Ok(meta) => {
                            times = (meta.modified.or(entry.modified), meta.created);
                            self.sidecar(
                                &folder_host,
                                &mut names,
                                &chosen,
                                entry.node,
                                meta.finder_info,
                                false,
                                &vol,
                                false,
                            )?;
                        }
                        Err(error) => self.fail(&vol, &error),
                    }
                }
                children.push(Frame::Enter {
                    node: entry.node,
                    host: path,
                    vol: vol.clone(),
                    depth: depth.saturating_add(1),
                    modified: times.0,
                    created: times.1,
                });
            }

            stack.push(Frame::Leave {
                node,
                host: folder_host,
                modified,
                created,
            });
            // Reversed, so the stack hands them out in listing order.
            stack.extend(children.drain(..).rev());
        }
        Ok(())
    }

    /// Lists `node` fully into `batch`. A failure is reported for the
    /// folder; what was listed before it stays in `batch`.
    fn list(&mut self, node: NodeId, vol: &str, batch: &mut DirBatch) {
        let mut stream = match self.fs.read_dir(node) {
            Ok(stream) => stream,
            Err(error) => {
                self.fail(vol, &error);
                return;
            }
        };
        let mut empty_in_a_row: u32 = 0;
        loop {
            let before = batch.len();
            match stream.next_batch(batch, LIST_BATCH) {
                Ok(DirProgress::Done) => break,
                Ok(DirProgress::More) if batch.len() == before => {
                    empty_in_a_row = empty_in_a_row.saturating_add(1);
                    if empty_in_a_row >= MAX_EMPTY_BATCHES {
                        let error = Error::from(Corruption::new(
                            "folder listing",
                            Location::Object(node.0),
                            CorruptKind::LimitExceeded {
                                what: "empty listing batches in a row",
                                limit: u64::from(MAX_EMPTY_BATCHES),
                            },
                        ));
                        self.fail(vol, &error);
                        break;
                    }
                }
                Ok(DirProgress::More) => empty_in_a_row = 0,
                Err(error) => {
                    self.fail(vol, &error);
                    break;
                }
            }
        }
    }

    /// False, with the skip reported, for a hidden entry the options leave out.
    fn wanted(&mut self, entry: &DirEntry<'_>, vol: &str) -> bool {
        if self.hidden_ok(entry) {
            true
        } else {
            self.skip(vol, Skip::Hidden);
            false
        }
    }

    fn hidden_ok(&self, entry: &DirEntry<'_>) -> bool {
        self.options.hidden || !entry.flags.contains(EntryFlags::HIDDEN)
    }

    /// Copies one file into `dir`, then its sidecar when forks are on.
    /// `meta` is the file's metadata when the caller has it. `in_dest` for
    /// a selected item, written directly in the destination.
    fn file(
        &mut self,
        dir: &Path,
        names: &mut NameSet,
        vol: &str,
        facts: FileFacts<'_>,
        meta: Option<Metadata>,
        in_dest: bool,
    ) -> Flow {
        (self.events)(CopyEvent::Started {
            name: facts.name,
            size: facts.size,
        });
        let source = match self.fs.open(facts.node, Stream::Data) {
            Ok(source) => source,
            Err(error) => {
                self.fail(vol, &error);
                return Ok(());
            }
        };
        let legal = self.host_name(facts.name);
        let Some((path, chosen, mut file)) =
            self.create_unique(dir, names, &legal, vol, in_dest, create_new)?
        else {
            return Ok(());
        };
        if let Err(stop) = self.stream_into(&*source, &mut file, &path, vol, in_dest) {
            return stop;
        }
        if chosen != facts.name {
            self.summary.renamed = self.summary.renamed.saturating_add(1);
        }

        let meta = match meta {
            Some(meta) => Some(meta),
            None if self.options.forks => match self.fs.metadata(facts.node) {
                Ok(meta) => Some(meta),
                Err(error) => {
                    self.fail(vol, &error);
                    None
                }
            },
            None => None,
        };
        let modified = meta.as_ref().and_then(|m| m.modified).or(facts.modified);
        let created = meta.as_ref().and_then(|m| m.created);
        if let Some(times) = file_times(modified, created) {
            // A time the host does not take is left as it is.
            let _ = file.set_times(times);
        }
        drop(file);
        self.summary.files = self.summary.files.saturating_add(1);
        if in_dest {
            self.summary.created.push(path);
        }
        if self.options.forks
            && let Some(meta) = meta
        {
            self.sidecar(
                dir,
                names,
                &chosen,
                facts.node,
                meta.finder_info,
                true,
                vol,
                in_dest,
            )?;
        }
        Ok(())
    }

    /// Streams `source` into the new host file `file` at `path`. On any
    /// failure the host file is removed; `Ok(Err(..))` carries what the
    /// caller returns.
    fn stream_into(
        &mut self,
        source: &dyn BlockSource,
        file: &mut File,
        path: &Path,
        vol: &str,
        in_dest: bool,
    ) -> std::result::Result<(), Flow> {
        match self.stream(source, file) {
            Ok(()) => Ok(()),
            Err(failure) => {
                // Truncate first so nothing readable stays if removal fails.
                let _ = file.set_len(0);
                let _ = host::remove_file(path);
                Err(match failure {
                    StreamError::Read(error) => {
                        self.fail(vol, &error);
                        Ok(())
                    }
                    StreamError::Write(error) => {
                        self.host_failure(vol, "writing", path, error, in_dest)
                    }
                    StreamError::Cancelled => Err(Stop::Cancelled),
                })
            }
        }
    }

    /// Copies all of `source` into `out` through the copy buffer.
    fn stream(
        &mut self,
        source: &dyn BlockSource,
        out: &mut File,
    ) -> std::result::Result<(), StreamError> {
        let len = source.len();
        let mut offset: u64 = 0;
        while let Some(left) = len.checked_sub(offset).filter(|&left| left > 0) {
            let step = usize::try_from(left).map_or(BUFFER_SIZE, |left| left.min(BUFFER_SIZE));
            let Some(chunk) = self.buf.get_mut(..step) else {
                return Err(StreamError::Write(io::Error::other(
                    "copy buffer too small",
                )));
            };
            source.read_bulk(offset, chunk).map_err(StreamError::Read)?;
            out.write_all(chunk).map_err(StreamError::Write)?;
            let step = u64::try_from(step).unwrap_or(u64::MAX);
            offset = offset.saturating_add(step);
            self.count(step);
            if offset < len && self.cancel.load(Ordering::Relaxed) {
                self.summary.cancelled = true;
                return Err(StreamError::Cancelled);
            }
        }
        if self.summary.bytes != self.reported {
            self.reported = self.summary.bytes;
            (self.events)(CopyEvent::Bytes {
                total_bytes: self.reported,
            });
        }
        Ok(())
    }

    /// Adds `n` written bytes, reporting each MiB.
    fn count(&mut self, n: u64) {
        self.summary.bytes = self.summary.bytes.saturating_add(n);
        if self.summary.bytes.saturating_sub(self.reported) >= MIB {
            self.reported = self.summary.bytes;
            (self.events)(CopyEvent::Bytes {
                total_bytes: self.reported,
            });
        }
    }

    /// Writes the AppleDouble sidecar `._<host_name>` in `dir` for `node`,
    /// when it has a resource fork (files only) or Finder information.
    #[allow(clippy::too_many_arguments)]
    fn sidecar(
        &mut self,
        dir: &Path,
        names: &mut NameSet,
        host_name: &str,
        node: NodeId,
        finder_info: Option<[u8; appledouble::FINDER_INFO_LEN]>,
        is_file: bool,
        vol: &str,
        in_dest: bool,
    ) -> Flow {
        let fork = if is_file {
            match self.fs.open(node, Stream::Resource) {
                Ok(fork) if !fork.is_empty() => Some(fork),
                Ok(_) | Err(Error::NoSuchStream { .. } | Error::Unsupported(_)) => None,
                Err(error) => {
                    self.fail(vol, &error);
                    return Ok(());
                }
            }
        } else {
            None
        };
        if fork.is_none() && finder_info.is_none() {
            return Ok(());
        }
        let fork_len = fork.as_ref().map_or(0, |fork| fork.len());
        let Some(header) = appledouble::header(finder_info.as_ref(), fork_len) else {
            self.skip(vol, Skip::ForkTooLarge);
            return Ok(());
        };

        let name = format!("._{host_name}");
        let fits =
            names::fits(&name, self.options.rules) && names::fits(&name, HostRules::current());
        let Some(path) = fits.then(|| join_component(dir, &name)).flatten() else {
            let error = Error::io(
                path_context("creating", &dir.join(&name)),
                io::Error::new(
                    io::ErrorKind::InvalidFilename,
                    "the sidecar name is too long",
                ),
            );
            self.fail(vol, &error);
            return Ok(());
        };
        // A renamed sidecar would belong to nothing, so a taken name is a failure.
        if !names.claim(&name) {
            let error = Error::io(
                path_context("creating", &path),
                io::Error::from(io::ErrorKind::AlreadyExists),
            );
            self.fail(vol, &error);
            return Ok(());
        }
        let mut file = match create_new(&path) {
            Ok(file) => file,
            Err(error) => return self.host_failure(vol, "creating", &path, error, in_dest),
        };
        if let Err(error) = file.write_all(&header) {
            let _ = host::remove_file(&path);
            return self.host_failure(vol, "writing", &path, error, in_dest);
        }
        if let Some(fork) = fork
            && let Err(stop) = self.stream_into(&*fork, &mut file, &path, vol, in_dest)
        {
            return stop;
        }
        if in_dest {
            self.summary.created.push(path);
        }
        Ok(())
    }

    /// Picks a free name for `legal` in `dir` and makes it with `make`,
    /// trying the next unique name while the host says it exists. `None`
    /// when it could not be made (the failure is reported).
    fn create_unique<T>(
        &mut self,
        dir: &Path,
        names: &mut NameSet,
        legal: &str,
        vol: &str,
        in_dest: bool,
        make: impl Fn(&Path) -> io::Result<T>,
    ) -> std::result::Result<Option<(PathBuf, String, T)>, Stop> {
        let rules = self.options.rules;
        let host = HostRules::current();
        let mut chosen = names.unique_for(legal, rules, host);
        let mut last = None;
        for _ in 0..MAX_NAME_TRIES {
            let Some(path) = join_component(dir, &chosen) else {
                // Not reached: sanitizing under the host's rules leaves one
                // plain component. Kept so a path can never leave `dir`.
                let error = Error::io(
                    path_context("creating", dir),
                    io::Error::new(io::ErrorKind::InvalidFilename, "not a plain file name"),
                );
                self.fail(vol, &error);
                return Ok(None);
            };
            match make(&path) {
                Ok(made) => return Ok(Some((path, chosen, made))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last = Some((path, error));
                    chosen = names.unique_for(legal, rules, host);
                }
                Err(error) => {
                    self.host_failure(vol, "creating", &path, error, in_dest)?;
                    return Ok(None);
                }
            }
        }
        if let Some((path, error)) = last {
            let error = Error::io(path_context("creating", &path), error);
            self.fail(vol, &error);
        }
        Ok(None)
    }

    /// `name` made legal under the chosen rules and then under the host's.
    fn host_name<'n>(&self, name: &'n str) -> Cow<'n, str> {
        let host = HostRules::current();
        let named = sanitize(name, self.options.rules);
        if host == self.options.rules {
            return named;
        }
        match named {
            Cow::Borrowed(named) => sanitize(named, host),
            Cow::Owned(named) => Cow::Owned(sanitize(&named, host).into_owned()),
        }
    }

    fn folder_made(&mut self, name: &str, chosen: &str) {
        self.summary.folders = self.summary.folders.saturating_add(1);
        if chosen != name {
            self.summary.renamed = self.summary.renamed.saturating_add(1);
        }
    }

    /// A host error at `path`: fatal when the disk is full or read-only,
    /// when the destination itself refuses writes or is gone; otherwise
    /// reported for `vol` and the copy goes on.
    fn host_failure(
        &mut self,
        vol: &str,
        op: &'static str,
        path: &Path,
        error: io::Error,
        in_dest: bool,
    ) -> Flow {
        let kind = error.kind();
        let fatal = matches!(
            kind,
            io::ErrorKind::StorageFull
                | io::ErrorKind::QuotaExceeded
                | io::ErrorKind::ReadOnlyFilesystem
        ) || (in_dest && kind == io::ErrorKind::PermissionDenied)
            || !host::metadata(&self.dest).is_ok_and(|meta| meta.is_dir());
        let error = Error::io(path_context(op, path), error);
        if fatal {
            Err(Stop::Fatal(error))
        } else {
            self.fail(vol, &error);
            Ok(())
        }
    }

    fn check_cancel(&mut self) -> Flow {
        if self.cancel.load(Ordering::Relaxed) {
            self.summary.cancelled = true;
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    fn fail(&mut self, vol: &str, error: &Error) {
        self.summary.failed = self.summary.failed.saturating_add(1);
        if self.summary.first_failures.len() < MAX_FIRST_FAILURES {
            self.summary
                .first_failures
                .push((vol.to_owned(), error.to_string()));
        }
        (self.events)(CopyEvent::Failed { path: vol, error });
    }

    fn skip(&mut self, vol: &str, why: Skip) {
        match why {
            Skip::Symlink => {
                self.summary.skipped_links = self.summary.skipped_links.saturating_add(1);
            }
            Skip::Special | Skip::ForkTooLarge | Skip::TooDeep | Skip::Hidden => {
                self.summary.skipped_other = self.summary.skipped_other.saturating_add(1);
            }
        }
        (self.events)(CopyEvent::Skipped { path: vol, why });
    }
}

/// Sets `out` to `parent/name`, the volume path of an entry.
fn volume_path(out: &mut String, parent: &str, name: &str) {
    out.clear();
    out.push_str(parent);
    out.push('/');
    out.push_str(name);
}

/// `dir/name` when `name` is exactly one plain path component on this
/// host: no separator, prefix, root, `.` or `..` can move the result out
/// of `dir`.
fn join_component(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(only)), None) if only == OsStr::new(name) => Some(dir.join(name)),
        _ => None,
    }
}

/// Opens a new file for writing; never opens or follows anything that is
/// already there.
fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn path_context(op: &'static str, path: &Path) -> IoContext {
    IoContext::Path {
        op,
        path: path.to_path_buf(),
    }
}

/// `time` as a host clock value, or `None` when the host cannot hold it.
fn system_time(time: Timestamp) -> Option<SystemTime> {
    let nanos = time.unix_nanos();
    let distance = Duration::from_nanos(nanos.unsigned_abs());
    if nanos >= 0 {
        UNIX_EPOCH.checked_add(distance)
    } else {
        UNIX_EPOCH.checked_sub(distance)
    }
}

/// The times to set, or `None` when there are none the host can hold. The
/// created time is set on Windows and macOS only.
fn file_times(modified: Option<Timestamp>, created: Option<Timestamp>) -> Option<FileTimes> {
    let modified = modified.and_then(system_time);
    let created = created.and_then(system_time);
    if modified.is_none() && created.is_none() {
        return None;
    }
    let mut times = FileTimes::new();
    if let Some(modified) = modified {
        times = times.set_modified(modified);
    }
    #[cfg(windows)]
    if let Some(created) = created {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(created);
    }
    #[cfg(target_os = "macos")]
    if let Some(created) = created {
        use std::os::macos::fs::FileTimesExt;
        times = times.set_created(created);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = created;
    Some(times)
}

/// Sets a folder's times, ignoring any failure.
fn set_dir_times(path: &Path, modified: Option<Timestamp>, created: Option<Timestamp>) {
    if let Some(times) = file_times(modified, created) {
        let _ = open_dir_for_times(path).and_then(|dir| dir.set_times(times));
    }
}

/// A handle to the folder `path` that may set its times.
#[cfg(windows)]
fn open_dir_for_times(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// A handle to the folder `path` that may set its times.
#[cfg(not(windows))]
fn open_dir_for_times(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use crate::fs::hfsplus::compression::fixtures::{content, header as decmpfs, zlib};
    use crate::fs::hfsplus::links::DIR_PRIVATE_FOLDER;
    use crate::fs::hfsplus::test_volume::{self, FileSpec, ROOT, VolumeBuilder};
    use crate::fs::hfsplus::{self, HfsPlus};
    use crate::fs::{DirStream, VolumeInfo};
    use crate::io::block::check_range;

    // ---- host side ---------------------------------------------------------

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh folder under the system temp folder, removed on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "disk-atlas-extract-{}-{n}-{tag}",
                std::process::id()
            ));
            let _ = host::remove_dir_all(&path);
            host::create_dir_all(&path).unwrap();
            Self(host::canonicalize(&path).unwrap())
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = host::remove_dir_all(&self.0);
        }
    }

    /// Everything under `dir`: relative path (with `/`) to the file's bytes,
    /// or `None` for a folder.
    fn tree(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![(dir.to_path_buf(), String::new())];
        while let Some((path, rel)) = stack.pop() {
            for entry in host::read_dir(&path).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let rel = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if entry.file_type().unwrap().is_dir() {
                    out.insert(rel.clone(), None);
                    stack.push((entry.path(), rel));
                } else {
                    out.insert(rel, Some(host::read(entry.path()).unwrap()));
                }
            }
        }
        out
    }

    fn mtime(path: &Path) -> SystemTime {
        host::metadata(path).unwrap().modified().unwrap()
    }

    /// The events of one run, owned.
    #[derive(Debug, Default)]
    struct Log {
        started: Vec<(String, Option<u64>)>,
        bytes: Vec<u64>,
        finished: Vec<u64>,
        failed: Vec<String>,
        skipped: Vec<(String, Skip)>,
    }

    impl Log {
        fn record(&mut self, event: CopyEvent<'_>) {
            match event {
                CopyEvent::Started { name, size } => self.started.push((name.into(), size)),
                CopyEvent::Bytes { total_bytes } => self.bytes.push(total_bytes),
                CopyEvent::Finished { total_files } => self.finished.push(total_files),
                CopyEvent::Failed { path, .. } => self.failed.push(path.into()),
                CopyEvent::Skipped { path, why } => self.skipped.push((path.into(), why)),
            }
        }
    }

    fn options(forks: bool) -> CopyOptions {
        CopyOptions {
            forks,
            ..CopyOptions::default()
        }
    }

    fn run(
        fs: &dyn FileSystem,
        items: &[Item],
        dest: &Path,
        options: &CopyOptions,
    ) -> (Result<CopySummary>, Log) {
        let cancel = AtomicBool::new(false);
        let mut log = Log::default();
        let result = copy_out(fs, items, dest, options, &cancel, &mut |e| log.record(e));
        (result, log)
    }

    fn item(node: u64, name: &str) -> Item {
        Item {
            node: NodeId(node),
            name: name.into(),
        }
    }

    /// Every entry of `dir` as a selected item.
    fn items_of(fs: &dyn FileSystem, dir: NodeId) -> Vec<Item> {
        let mut stream = fs.read_dir(dir).unwrap();
        let mut batch = DirBatch::new();
        while stream.next_batch(&mut batch, 64).unwrap() == DirProgress::More {}
        batch
            .iter()
            .map(|e| Item {
                node: e.node,
                name: e.name.into(),
            })
            .collect()
    }

    // ---- a volume in memory ------------------------------------------------

    const MODIFIED: Timestamp = Timestamp::from_unix(1_600_000_000, 0);
    const CREATED: Timestamp = Timestamp::from_unix(1_500_000_000, 0);

    #[derive(Clone)]
    enum MockKind {
        Dir(Vec<(String, u64)>),
        File {
            data: Arc<[u8]>,
            /// Resource fork length; its bytes are a pattern.
            fork: u64,
            /// Reads that reach this byte fail.
            bad_from: Option<u64>,
        },
        Link,
        Special,
    }

    #[derive(Clone)]
    struct MockNode {
        kind: MockKind,
        flags: EntryFlags,
        finder: Option<[u8; 32]>,
    }

    impl MockNode {
        fn kind(&self) -> NodeKind {
            match self.kind {
                MockKind::Dir(_) => NodeKind::Directory,
                MockKind::File { .. } => NodeKind::File,
                MockKind::Link => NodeKind::Symlink,
                MockKind::Special => NodeKind::Other,
            }
        }

        fn size(&self) -> Option<u64> {
            match &self.kind {
                MockKind::File { data, .. } => Some(data.len() as u64),
                _ => None,
            }
        }

        fn fork(&self) -> Option<u64> {
            match &self.kind {
                MockKind::File { fork, .. } => Some(*fork),
                _ => None,
            }
        }
    }

    /// A tree of nodes; node 1 is the root.
    struct Mock {
        nodes: HashMap<u64, MockNode>,
        /// (folder, n): listing the folder fails after n entries.
        fail_listing: Option<(u64, usize)>,
    }

    impl Mock {
        fn new() -> Self {
            let mut nodes = HashMap::new();
            nodes.insert(1, Self::node(MockKind::Dir(Vec::new())));
            Self {
                nodes,
                fail_listing: None,
            }
        }

        fn node(kind: MockKind) -> MockNode {
            MockNode {
                kind,
                flags: EntryFlags::EMPTY,
                finder: None,
            }
        }

        /// Lists `id` in `parent` as `name`; `node` is used if `id` is new.
        fn add(&mut self, parent: u64, name: &str, id: u64, node: MockNode) -> &mut Self {
            self.nodes.entry(id).or_insert(node);
            match &mut self.nodes.get_mut(&parent).unwrap().kind {
                MockKind::Dir(children) => children.push((name.into(), id)),
                _ => panic!("{parent} is not a folder"),
            }
            self
        }

        fn dir(&mut self, parent: u64, name: &str, id: u64) -> &mut Self {
            self.add(parent, name, id, Self::node(MockKind::Dir(Vec::new())))
        }

        fn file(&mut self, parent: u64, name: &str, id: u64, data: &[u8]) -> &mut Self {
            let kind = MockKind::File {
                data: data.into(),
                fork: 0,
                bad_from: None,
            };
            self.add(parent, name, id, Self::node(kind))
        }

        fn with(&mut self, id: u64, f: impl FnOnce(&mut MockNode)) -> &mut Self {
            f(self.nodes.get_mut(&id).unwrap());
            self
        }

        fn get(&self, node: NodeId) -> Result<&MockNode> {
            self.nodes
                .get(&node.0)
                .ok_or(Error::NotFound { node: node.0 })
        }
    }

    struct MockStream {
        batch: DirBatch,
        at: usize,
        fail_after: Option<usize>,
    }

    impl DirStream for MockStream {
        fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
            for _ in 0..max {
                if Some(self.at) == self.fail_after {
                    return Err(damage("listing broke"));
                }
                let Some(entry) = self.batch.get(self.at) else {
                    return Ok(DirProgress::Done);
                };
                out.push(entry);
                self.at += 1;
            }
            Ok(DirProgress::More)
        }
    }

    fn damage(text: &'static str) -> Error {
        Corruption::new("mock", Location::Unspecified, CorruptKind::Other(text)).into()
    }

    impl FileSystem for Mock {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("Mock")
        }

        fn root(&self) -> NodeId {
            NodeId(1)
        }

        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            let n = self.get(node)?;
            Ok(Metadata {
                node,
                kind: n.kind(),
                size: n.size(),
                resource_size: n.fork(),
                created: Some(CREATED),
                modified: Some(MODIFIED),
                accessed: None,
                flags: n.flags,
                link_target: None,
                finder_codes: None,
                owner: None,
                attributes: None,
                finder_info: n.finder,
            })
        }

        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }

        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            let MockKind::Dir(children) = &self.get(dir)?.kind else {
                return Err(Error::NotADirectory { node: dir.0 });
            };
            let mut batch = DirBatch::new();
            for (name, id) in children {
                let n = &self.nodes[id];
                batch.push(DirEntry {
                    node: NodeId(*id),
                    name,
                    kind: n.kind(),
                    size: n.size(),
                    resource_size: n.fork(),
                    modified: Some(MODIFIED),
                    flags: n.flags,
                });
            }
            let fail_after = self
                .fail_listing
                .filter(|&(folder, _)| folder == dir.0)
                .map(|(_, n)| n);
            Ok(Box::new(MockStream {
                batch,
                at: 0,
                fail_after,
            }))
        }

        fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
            let n = self.get(node)?;
            match (&n.kind, stream) {
                (MockKind::File { data, bad_from, .. }, Stream::Data) => Ok(Arc::new(Damaged {
                    data: data.clone(),
                    bad_from: *bad_from,
                })),
                (MockKind::File { fork, .. }, Stream::Resource) if *fork > 0 => {
                    Ok(Arc::new(Pattern(*fork)))
                }
                (MockKind::Dir(_), _) => Err(Error::IsADirectory { node: node.0 }),
                _ => Err(Error::NoSuchStream { node: node.0 }),
            }
        }
    }

    /// File content whose reads fail from `bad_from` on.
    struct Damaged {
        data: Arc<[u8]>,
        bad_from: Option<u64>,
    }

    impl BlockSource for Damaged {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            check_range(offset, buf.len(), self.len())?;
            let end = offset + buf.len() as u64;
            if self.bad_from.is_some_and(|bad| end > bad) {
                return Err(damage("damaged extent"));
            }
            buf.copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(())
        }
    }

    /// `len` bytes of a pattern, without holding them.
    struct Pattern(u64);

    fn pattern_byte(at: u64) -> u8 {
        (at % 251) as u8
    }

    impl BlockSource for Pattern {
        fn len(&self) -> u64 {
            self.0
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            check_range(offset, buf.len(), self.0)?;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = pattern_byte(offset + i as u64);
            }
            Ok(())
        }
    }

    fn system(ts: Timestamp) -> SystemTime {
        system_time(ts).unwrap()
    }

    // ---- HFS+ volumes ------------------------------------------------------

    fn mounted(v: &VolumeBuilder) -> HfsPlus {
        hfsplus::mount(test_volume::source(v.build())).unwrap()
    }

    fn with_content(bytes: &[u8]) -> FileSpec {
        FileSpec {
            content: Some(bytes.to_vec()),
            ..FileSpec::regular(0)
        }
    }

    #[test]
    fn hfs_tree_is_copied_file_by_file() {
        let rules = HostRules::current();
        let mut v = VolumeBuilder::new("Copy");
        let big = content(BUFFER_SIZE + 300_000);
        let notes = content(5000);
        let inline = content(3000);
        let mut attr = decmpfs(3, inline.len() as u64);
        attr.extend_from_slice(&zlib(&inline));
        v.file(ROOT, "readme.txt", 16, with_content(b"hello"))
            .folder(ROOT, "Docs", 20)
            .folder(20, "Sub", 21)
            .folder(ROOT, "Empty", 23)
            .file(20, "notes.txt", 24, with_content(&notes))
            .file(21, "big.bin", 25, with_content(&big))
            .file(20, "a:b?.txt", 26, with_content(b"odd"))
            .file(ROOT, "empty.dat", 28, FileSpec::regular(0))
            .file(ROOT, "Trail. ", 30, with_content(b"t"))
            .compressed_file(ROOT, "inline.txt", 29, &attr, None)
            .file(ROOT, "link", 52, FileSpec::symlink(b"Docs/notes.txt"));
        let fs = mounted(&v);
        let items = items_of(&fs, fs.root());
        let temp = Temp::new("tree");

        let (result, log) = run(&fs, &items, &temp.0, &options(false));
        let summary = result.unwrap();

        let odd = sanitize("a:b?.txt", rules).into_owned();
        let trail = sanitize("Trail. ", rules).into_owned();
        let mut want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        want.insert("readme.txt".into(), Some(b"hello".to_vec()));
        want.insert("Docs".into(), None);
        want.insert("Docs/Sub".into(), None);
        want.insert("Empty".into(), None);
        want.insert("Docs/notes.txt".into(), Some(notes));
        want.insert("Docs/Sub/big.bin".into(), Some(big.clone()));
        want.insert(format!("Docs/{odd}"), Some(b"odd".to_vec()));
        want.insert("empty.dat".into(), Some(Vec::new()));
        want.insert(trail.clone(), Some(b"t".to_vec()));
        want.insert("inline.txt".into(), Some(inline));
        assert_eq!(tree(&temp.0), want);

        let renamed = u64::from(odd != "a:b?.txt") + u64::from(trail != "Trail. ");
        assert_eq!(summary.files, 7);
        assert_eq!(summary.folders, 3);
        assert_eq!(summary.skipped_links, 1);
        assert_eq!(summary.skipped_other, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.renamed, renamed);
        assert!(!summary.cancelled);
        assert_eq!(summary.created.len(), 6);
        assert!(summary.created.iter().all(|p| p.parent() == Some(&*temp.0)));
        assert_eq!(log.skipped, vec![("link".to_string(), Skip::Symlink)]);
        assert_eq!(log.finished, vec![7]);
        assert_eq!(log.started.len(), 7);
        assert!(
            log.started
                .contains(&("big.bin".into(), Some(big.len() as u64)))
        );
        assert!(log.bytes.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(log.bytes.last(), Some(&summary.bytes));
        assert!(log.bytes.contains(&(BUFFER_SIZE as u64)) || log.bytes.iter().any(|&b| b >= MIB));

        // Times come from the volume, folders included (set after their content).
        let want_time = |node: u64| system(fs.metadata(NodeId(node)).unwrap().modified.unwrap());
        assert_eq!(mtime(&temp.0.join("readme.txt")), want_time(16));
        assert_eq!(
            mtime(&temp.0.join("Docs").join("Sub").join("big.bin")),
            want_time(25)
        );
        assert_eq!(mtime(&temp.0.join("Docs")), want_time(20));
        assert_eq!(mtime(&temp.0.join("Docs").join("Sub")), want_time(21));
    }

    #[test]
    fn hfs_sidecars_for_forks_and_finder_info() {
        const FORK: &[u8] = b"resource fork bytes";
        let mut v = VolumeBuilder::new("Forks");
        let inline = content(100);
        let mut attr = decmpfs(3, inline.len() as u64);
        attr.extend_from_slice(&zlib(&inline));
        v.file(ROOT, "forked", 16, with_content(b"data"))
            .resource_fork(16, FORK)
            .file(
                ROOT,
                "bare",
                17,
                FileSpec {
                    file_type: 0,
                    creator: 0,
                    ..with_content(b"x")
                },
            )
            .compressed_file(ROOT, "packed", 18, &attr, None)
            .folder_units(ROOT, &test_volume::units("Shown"), 19, 0x4000)
            .file(19, "inner", 20, with_content(b"in"))
            .resource_fork(20, b"rf");
        let fs = mounted(&v);
        let items = items_of(&fs, fs.root());

        // Without forks: no sidecars at all.
        let plain = Temp::new("noforks");
        let (result, _) = run(&fs, &items, &plain.0, &options(false));
        result.unwrap();
        assert!(tree(&plain.0).keys().all(|k| !k.contains("._")));

        let temp = Temp::new("forks");
        let (result, log) = run(&fs, &items, &temp.0, &options(true));
        let summary = result.unwrap();
        assert_eq!(summary.failed, 0, "{:?}", summary.first_failures);
        let got = tree(&temp.0);

        let info = |node: u64| fs.metadata(NodeId(node)).unwrap().finder_info.unwrap();
        let forked = got["._forked"].clone().unwrap();
        let mut want = appledouble::header(Some(&info(16)), FORK.len() as u64).unwrap();
        want.extend_from_slice(FORK);
        assert_eq!(forked, want);
        // Spelled out: two entries, Finder info at 50, the fork at 82.
        assert_eq!(&forked[..4], &[0x00, 0x05, 0x16, 0x07]);
        assert_eq!(&forked[24..26], &[0, 2]);
        assert_eq!(&forked[50..58], b"TEXTttxt");
        assert_eq!(&forked[82..], FORK);
        assert_eq!(got["forked"], Some(b"data".to_vec()));

        assert!(!got.contains_key("._bare"));
        // A compressed file: Finder info only, no fork entry.
        let packed = got["._packed"].clone().unwrap();
        assert_eq!(packed, appledouble::header(Some(&info(18)), 0).unwrap());
        assert_eq!(packed.len(), 70);
        assert_eq!(got["packed"], Some(inline));
        // A folder's sidecar sits next to it; its DInfo flags at 38 + 8.
        let shown = got["._Shown"].clone().unwrap();
        assert_eq!(shown.len(), 70);
        assert_eq!(&shown[46..48], &0x4000u16.to_be_bytes());
        let inner = got["Shown/._inner"].clone().unwrap();
        assert_eq!(&inner[82..], b"rf");
        // Fork bytes count as written bytes.
        assert_eq!(summary.bytes, (4 + 1 + 100 + 2 + FORK.len() + 2) as u64);
        assert_eq!(summary.files, 4);
        assert!(summary.created.iter().any(|p| p.ends_with("._forked")));
        assert!(log.failed.is_empty());
    }

    #[test]
    fn hfs_directory_link_cycle_terminates() {
        let mut v = VolumeBuilder::new("Cycle");
        v.folder_units(ROOT, &DIR_PRIVATE_FOLDER, 40, 0x5000)
            .folder(40, "dir_200", 41)
            .file(41, "file.txt", 42, with_content(b"f"))
            .file(41, "loop", 60, FileSpec::dir_link(200))
            .file(ROOT, "entry", 61, FileSpec::dir_link(200));
        let fs = mounted(&v);
        let items = items_of(&fs, fs.root());
        let temp = Temp::new("hfscycle");
        let (result, _) = run(&fs, &items, &temp.0, &options(true));
        let summary = result.unwrap();
        // Either the cycle guard or the depth bound ends it.
        assert!(summary.failed + summary.skipped_other >= 1, "{summary:?}");
        assert!(summary.folders <= MAX_DEPTH as u64);
        assert!(tree(&temp.0).contains_key("entry/file.txt"));
    }

    // ---- the walk ----------------------------------------------------------

    #[test]
    fn names_cannot_leave_their_folder() {
        let hostile = [
            "../x",
            "..\\x",
            "C:",
            "/etc/passwd",
            "a\0b",
            "..",
            ".",
            "",
            "CON",
        ];
        for rules in [HostRules::Windows, HostRules::Unix, HostRules::MacOs] {
            let mut fs = Mock::new();
            fs.dir(1, "top", 2);
            for (i, name) in hostile.iter().enumerate() {
                fs.file(2, name, 10 + i as u64, name.as_bytes());
                fs.dir(2, name, 30 + i as u64);
                fs.file(30 + i as u64, name, 50 + i as u64, b"deep");
            }
            fs.file(1, "loose", 3, b"loose");
            let temp = Temp::new("escape");
            let dest = temp.0.join("dest");
            host::create_dir(&dest).unwrap();
            let items = [
                item(2, "../top"),
                item(3, "..\\..\\loose"),
                item(3, "C:\\loose"),
            ];
            let options = CopyOptions {
                forks: false,
                hidden: true,
                rules,
            };
            let (result, _) = run(&fs, &items, &dest, &options);
            let summary = result.unwrap();
            assert_eq!(summary.failed, 0, "{rules:?} {:?}", summary.first_failures);
            assert_eq!(summary.files, 2 * hostile.len() as u64 + 2, "{rules:?}");

            // Nothing outside `dest`.
            let outside: Vec<_> = host::read_dir(&temp.0)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(outside, vec![std::ffi::OsString::from("dest")], "{rules:?}");
            // Three top-level entries, each directly in `dest`.
            assert_eq!(summary.created.len(), 3);
            for path in &summary.created {
                assert_eq!(path.parent(), Some(&*dest), "{rules:?} {path:?}");
            }
            let top = &summary.created[0];
            // Every file and folder of `top` is directly in it, and every
            // folder holds exactly its one file.
            let mut folders = 0;
            for entry in host::read_dir(top).unwrap() {
                let entry = entry.unwrap();
                assert_eq!(entry.path().parent(), Some(top.as_path()));
                if entry.file_type().unwrap().is_dir() {
                    folders += 1;
                    let inside: Vec<_> = host::read_dir(entry.path()).unwrap().collect();
                    assert_eq!(inside.len(), 1);
                }
            }
            assert_eq!(folders, hostile.len(), "{rules:?}");
        }
    }

    #[test]
    fn directory_cycle_terminates_and_shared_folders_copy_twice() {
        let mut fs = Mock::new();
        fs.dir(1, "A", 10)
            .dir(10, "B", 11)
            .add(11, "loop", 10, Mock::node(MockKind::Link))
            .dir(10, "C1", 12)
            .add(11, "C2", 12, Mock::node(MockKind::Link))
            .file(12, "f", 13, b"shared");
        let temp = Temp::new("cycle");
        let (result, log) = run(&fs, &[item(10, "A")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.first_failures[0].0, "A/B/loop");
        assert!(
            summary.first_failures[0].1.contains("loop"),
            "{:?}",
            summary.first_failures
        );
        assert_eq!(log.failed, vec!["A/B/loop".to_string()]);
        let got = tree(&temp.0);
        let keys: Vec<&str> = got.keys().map(String::as_str).collect();
        assert_eq!(keys, ["A", "A/B", "A/B/C2", "A/B/C2/f", "A/C1", "A/C1/f"]);
        assert_eq!(summary.folders, 4);
        assert_eq!(summary.files, 2);
    }

    #[test]
    fn stacked_folder_links_are_bounded_by_visits() {
        // Every level holds two routes to the next: 2^40 walks without a bound.
        const LEVELS: u64 = 40;
        let mut fs = Mock::new();
        fs.dir(1, "top", 100);
        for i in 0..LEVELS {
            fs.dir(100 + i, "x", 101 + i)
                .add(100 + i, "y", 101 + i, Mock::node(MockKind::Link));
        }
        let temp = Temp::new("visits");
        let (result, _) = run(&fs, &[item(100, "top")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert!(summary.failed >= 1, "{summary:?}");
        assert!(
            summary.folders <= u64::from(MAX_FOLDER_VISITS) * (LEVELS + 1),
            "{} folders",
            summary.folders
        );
        assert!(!summary.cancelled);
    }

    #[test]
    fn nesting_stops_at_max_depth() {
        let mut fs = Mock::new();
        fs.dir(1, "d", 100);
        for id in 100..300 {
            fs.dir(id, "d", id + 1);
        }
        fs.file(300, "bottom", 999, b"x");
        let temp = Temp::new("deep");
        let (result, log) = run(&fs, &[item(100, "d")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.folders, MAX_DEPTH as u64);
        assert_eq!(summary.skipped_other, 1);
        let deepest = vec!["d"; MAX_DEPTH + 1].join("/");
        assert_eq!(log.skipped, vec![(deepest, Skip::TooDeep)]);
        let got = tree(&temp.0);
        assert_eq!(got.len(), MAX_DEPTH);
        assert!(got.keys().all(|k| k.split('/').count() <= MAX_DEPTH));
    }

    #[test]
    fn cancel_mid_file_leaves_no_partial_file() {
        let mut fs = Mock::new();
        fs.file(1, "big", 2, &vec![7u8; 3 * BUFFER_SIZE + 5]);
        fs.file(1, "after", 3, b"never");
        let temp = Temp::new("cancel");
        let cancel = AtomicBool::new(false);
        let mut log = Log::default();
        let summary = copy_out(
            &fs,
            &[item(2, "big"), item(3, "after")],
            &temp.0,
            &options(false),
            &cancel,
            &mut |event| {
                if let CopyEvent::Bytes { .. } = event {
                    cancel.store(true, Ordering::Relaxed);
                }
                log.record(event);
            },
        )
        .unwrap();
        assert!(summary.cancelled);
        assert_eq!(summary.files, 0);
        assert_eq!(log.bytes, vec![BUFFER_SIZE as u64]);
        assert_eq!(log.finished, vec![0]);
        assert!(tree(&temp.0).is_empty());

        // Cancelled before the start: nothing at all.
        let (summary, events) = {
            let cancel = AtomicBool::new(true);
            let mut log = Log::default();
            let s = copy_out(
                &fs,
                &[item(3, "after")],
                &temp.0,
                &options(false),
                &cancel,
                &mut |e| log.record(e),
            );
            (s.unwrap(), log)
        };
        assert!(summary.cancelled);
        assert!(events.started.is_empty());
        assert!(tree(&temp.0).is_empty());
    }

    #[test]
    fn existing_names_in_dest_get_a_number() {
        let mut fs = Mock::new();
        fs.file(1, "Report.txt", 2, b"new")
            .dir(1, "folder", 3)
            .file(3, "in", 4, b"i")
            .file(1, "report.txt", 5, b"newer");
        let temp = Temp::new("exists");
        host::write(temp.0.join("report.txt"), b"old").unwrap();
        host::create_dir(temp.0.join("Folder")).unwrap();
        let items = [
            item(2, "Report.txt"),
            item(3, "folder"),
            item(5, "report.txt"),
        ];
        let (result, _) = run(&fs, &items, &temp.0, &options(false));
        let summary = result.unwrap();
        let got = tree(&temp.0);
        assert_eq!(got["report.txt"], Some(b"old".to_vec()));
        assert_eq!(got["Report (2).txt"], Some(b"new".to_vec()));
        assert_eq!(got["report (3).txt"], Some(b"newer".to_vec()));
        assert_eq!(got["folder (2)/in"], Some(b"i".to_vec()));
        assert_eq!(got["Folder"], None);
        assert_eq!(summary.renamed, 3);
        assert_eq!(summary.created.len(), 3);
    }

    #[test]
    fn damaged_file_leaves_no_host_file() {
        let mut fs = Mock::new();
        fs.dir(1, "top", 2)
            .file(2, "bad.bin", 3, &vec![1u8; 3 * BUFFER_SIZE])
            .with(3, |n| {
                if let MockKind::File { bad_from, .. } = &mut n.kind {
                    *bad_from = Some(BUFFER_SIZE as u64 + 10);
                }
            })
            .file(2, "tiny", 4, b"0123456789")
            .with(4, |n| {
                if let MockKind::File { bad_from, .. } = &mut n.kind {
                    *bad_from = Some(0);
                }
            })
            .file(2, "good.txt", 5, b"fine");
        let temp = Temp::new("damaged");
        let (result, log) = run(&fs, &[item(2, "top")], &temp.0, &options(false));
        let summary = result.unwrap();
        let keys: Vec<String> = tree(&temp.0).into_keys().collect();
        assert_eq!(keys, ["top", "top/good.txt"]);
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.first_failures[0].0, "top/bad.bin");
        assert!(summary.first_failures[0].1.contains("damaged extent"));
        assert_eq!(summary.first_failures[1].0, "top/tiny");
        assert_eq!(log.failed, ["top/bad.bin", "top/tiny"]);
    }

    #[test]
    fn first_failures_stop_at_a_hundred() {
        let mut fs = Mock::new();
        fs.dir(1, "top", 2);
        for i in 0..150u64 {
            fs.file(2, &format!("f{i}"), 10 + i, b"x")
                .with(10 + i, |n| {
                    if let MockKind::File { bad_from, .. } = &mut n.kind {
                        *bad_from = Some(0);
                    }
                });
        }
        let temp = Temp::new("many");
        let (result, log) = run(&fs, &[item(2, "top")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.failed, 150);
        assert_eq!(summary.first_failures.len(), MAX_FIRST_FAILURES);
        assert_eq!(log.failed.len(), 150);
    }

    #[test]
    fn hidden_entries_follow_the_option() {
        let mut fs = Mock::new();
        fs.dir(1, "top", 2)
            .file(2, "seen", 3, b"s")
            .file(2, "secret", 4, b"h")
            .with(4, |n| n.flags = EntryFlags::HIDDEN)
            .dir(2, "attic", 5)
            .with(5, |n| n.flags = EntryFlags::HIDDEN)
            .file(5, "box", 6, b"b");
        let temp = Temp::new("hidden");
        let leave_out = CopyOptions {
            hidden: false,
            ..options(false)
        };
        let (result, log) = run(&fs, &[item(2, "top")], &temp.0, &leave_out);
        let summary = result.unwrap();
        assert_eq!(summary.skipped_other, 2);
        assert_eq!(
            log.skipped,
            [
                ("top/secret".to_string(), Skip::Hidden),
                ("top/attic".to_string(), Skip::Hidden)
            ]
        );
        let keys: Vec<String> = tree(&temp.0).into_keys().collect();
        assert_eq!(keys, ["top", "top/seen"]);

        let all = Temp::new("hidden-all");
        let (result, _) = run(&fs, &[item(2, "top")], &all.0, &options(false));
        assert_eq!(result.unwrap().files, 3);
        // A selected item is copied even when hidden.
        let picked = Temp::new("hidden-picked");
        let (result, _) = run(&fs, &[item(4, "secret")], &picked.0, &leave_out);
        assert_eq!(result.unwrap().files, 1);
    }

    #[test]
    fn links_and_special_files_are_skipped() {
        let mut fs = Mock::new();
        fs.dir(1, "top", 2)
            .add(2, "link", 3, Mock::node(MockKind::Link))
            .add(2, "fifo", 4, Mock::node(MockKind::Special));
        let temp = Temp::new("special");
        let items = [item(2, "top"), item(3, "link"), item(4, "fifo")];
        let (result, log) = run(&fs, &items, &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.skipped_links, 2);
        assert_eq!(summary.skipped_other, 2);
        assert_eq!(
            log.skipped,
            [
                ("top/link".to_string(), Skip::Symlink),
                ("top/fifo".to_string(), Skip::Special),
                ("link".to_string(), Skip::Symlink),
                ("fifo".to_string(), Skip::Special),
            ]
        );
        assert_eq!(summary.created.len(), 1);
    }

    #[test]
    fn listing_error_keeps_what_was_listed() {
        let mut fs = Mock::new();
        fs.dir(1, "top", 2);
        for i in 0..4u64 {
            fs.file(2, &format!("f{i}"), 10 + i, b"x");
        }
        fs.fail_listing = Some((2, 2));
        let temp = Temp::new("listing");
        let (result, _) = run(&fs, &[item(2, "top")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.first_failures[0].0, "top");
        let keys: Vec<String> = tree(&temp.0).into_keys().collect();
        assert_eq!(keys, ["top", "top/f0", "top/f1"]);
    }

    #[test]
    fn mock_sidecars_and_their_limits() {
        let mut finder = [0u8; 32];
        finder[..8].copy_from_slice(b"APPLMACS");
        let mut fs = Mock::new();
        fs.dir(1, "top", 2)
            // A real `._x` listed first takes the name; x's sidecar fails.
            .file(2, "._x", 3, b"real")
            .file(2, "x", 4, b"x")
            .with(4, |n| n.finder = Some(finder))
            // A fork with no Finder info: zeros in its place.
            .file(2, "forked", 5, b"f")
            .with(5, |n| {
                if let MockKind::File { fork, .. } = &mut n.kind {
                    *fork = 3 * BUFFER_SIZE as u64 / 2;
                }
            })
            // A fork too long to describe: the file is copied, not the sidecar.
            .file(2, "huge", 6, b"h")
            .with(6, |n| {
                if let MockKind::File { fork, .. } = &mut n.kind {
                    *fork = appledouble::MAX_FORK_LEN + 1;
                }
            });
        let temp = Temp::new("mockforks");
        let (result, log) = run(&fs, &[item(2, "top")], &temp.0, &options(true));
        let summary = result.unwrap();
        let got = tree(&temp.0);
        assert_eq!(got["top/._x"], Some(b"real".to_vec()));
        assert_eq!(got["top/x"], Some(b"x".to_vec()));
        assert_eq!(summary.failed, 1);
        assert_eq!(log.failed, ["top/x"]);
        assert_eq!(log.skipped, [("top/huge".to_string(), Skip::ForkTooLarge)]);
        assert!(!got.contains_key("top/._huge"));
        assert_eq!(got["top/huge"], Some(b"h".to_vec()));

        let fork_len = 3 * BUFFER_SIZE as u64 / 2;
        let sidecar = got["top/._forked"].clone().unwrap();
        let mut want = appledouble::header(None, fork_len).unwrap();
        want.extend((0..fork_len).map(pattern_byte));
        assert_eq!(sidecar, want);
        assert_eq!(summary.files, 4);

        // Times on a copied file: modified always; created where the host
        // records it and metadata was read (forks on).
        let x = temp.0.join("top").join("x");
        assert_eq!(mtime(&x), system(MODIFIED));
        #[cfg(any(windows, target_os = "macos"))]
        assert_eq!(
            host::metadata(&x).unwrap().created().unwrap(),
            system(CREATED)
        );
        assert_eq!(mtime(&temp.0.join("top")), system(MODIFIED));
    }

    #[test]
    fn dest_must_be_an_existing_folder() {
        let fs = Mock::new();
        let temp = Temp::new("dest");
        let missing = temp.0.join("missing");
        let (result, log) = run(&fs, &[], &missing, &options(false));
        assert!(result.is_err());
        assert!(log.finished.is_empty());
        let file = temp.0.join("file");
        host::write(&file, b"x").unwrap();
        let (result, _) = run(&fs, &[], &file, &options(false));
        assert!(result.is_err());
        // An empty selection into a good folder finishes with nothing done.
        let (result, log) = run(&fs, &[], &temp.0, &options(false));
        assert_eq!(result.unwrap(), CopySummary::default());
        assert_eq!(log.finished, vec![0]);
    }

    #[test]
    fn missing_item_is_a_failure_not_an_error() {
        let fs = Mock::new();
        let temp = Temp::new("missing-item");
        let (result, _) = run(&fs, &[item(77, "ghost")], &temp.0, &options(false));
        let summary = result.unwrap();
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.first_failures[0].0, "ghost");
    }

    #[test]
    fn times_outside_the_host_range_are_skipped() {
        assert!(system_time(Timestamp::from_unix_nanos(i64::MAX)).is_some());
        // `SystemTime` itself holds times before 1601 even on Windows; it is the
        // host call that refuses them, and that failure is ignored by design.
        let early = Timestamp::from_unix(-12_000_000_000, 0);
        assert!(file_times(Some(early), None).is_some());
        assert!(file_times(None, None).is_none());
        assert_eq!(
            system_time(Timestamp::from_unix(5, 0)),
            Some(UNIX_EPOCH + Duration::from_secs(5))
        );
    }

    #[test]
    fn join_component_refuses_anything_but_one_plain_name() {
        let dir = Path::new("base");
        assert_eq!(join_component(dir, "a.txt"), Some(dir.join("a.txt")));
        for bad in ["", ".", "..", "a/b", "/abs"] {
            assert_eq!(join_component(dir, bad), None, "{bad:?}");
        }
        if cfg!(windows) {
            for bad in ["a\\b", "C:", "C:x", "\\\\server\\share"] {
                assert_eq!(join_component(dir, bad), None, "{bad:?}");
            }
        }
    }
}
