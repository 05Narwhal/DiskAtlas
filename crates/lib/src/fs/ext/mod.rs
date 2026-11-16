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

//! ext2, ext3 and ext4, read-only, through the `ext4-view` crate.
//!
//! The crate's `Ext4` handle is built on `Rc` and `RefCell`, so it cannot be
//! shared between threads. It lives on one worker thread of its own; every
//! call here sends a job to that thread and waits for the answer. This also
//! keeps a panic in the crate away from the caller: the worker catches it and
//! the call fails with an error, as a parser must.
//!
//! The crate addresses files by path, so a node id is an index into a table
//! of paths kept by the worker (the root is 0). The journal is replayed in
//! memory by the crate; nothing is ever written.
//!
//! Not supported: extended attributes, encrypted files, and block-level
//! extents (so no selection marks on the disk strip).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use ext4_view::{Ext4, Ext4Error, Ext4Read, FileType, ReadDir};

use crate::error::{CorruptKind, Corruption, Error, Location, Result};
use crate::fs::{
    DirBatch, DirEntry, DirProgress, DirStream, EntryFlags, FileSystem, Metadata, NodeId, NodeKind,
    Owner, Stream, Timestamp, VolumeIdentity, VolumeInfo,
};
use crate::io::block::{BlockSource, MemSource, check_range};

#[cfg(test)]
mod fuzz_seeds;
#[cfg(test)]
pub mod test_image;
#[cfg(test)]
mod tests;

const STRUCTURE: &str = "ext structure";
/// Most paths the node table holds.
const MAX_NODES: usize = 50_000_000;
/// Largest symbolic link target read.
const MAX_LINK_BYTES: usize = 65_536;

type Job = Box<dyn FnOnce(&mut State) + Send>;

/// One row of a listing batch, collected on the worker.
type Row = (u64, String, NodeKind, EntryFlags, Option<(u64, Timestamp)>);

/// What the worker thread owns.
struct State {
    fs: Ext4,
    paths: Vec<Vec<u8>>,
    ids: HashMap<Vec<u8>, u64>,
    streams: HashMap<u64, ReadDir>,
    next_stream: u64,
}

impl State {
    fn intern(&mut self, path: &[u8]) -> Result<u64> {
        if let Some(&id) = self.ids.get(path) {
            return Ok(id);
        }
        if self.paths.len() >= MAX_NODES {
            return Err(limit("node table", MAX_NODES as u64));
        }
        let id = self.paths.len() as u64;
        self.paths.push(path.to_vec());
        self.ids.insert(path.to_vec(), id);
        Ok(id)
    }

    fn path(&self, node: u64) -> Result<Vec<u8>> {
        usize::try_from(node)
            .ok()
            .and_then(|i| self.paths.get(i))
            .cloned()
            .ok_or(Error::NotFound { node })
    }
}

fn limit(what: &'static str, limit: u64) -> Error {
    Corruption::new(
        STRUCTURE,
        Location::Unspecified,
        CorruptKind::LimitExceeded { what, limit },
    )
    .into()
}

fn other(what: &'static str) -> Error {
    Corruption::new(STRUCTURE, Location::Unspecified, CorruptKind::Other(what)).into()
}

/// Maps the crate's error to ours; `node` is the node the call was about.
fn map_err(e: Ext4Error, node: u64) -> Error {
    match e {
        Ext4Error::NotFound => Error::NotFound { node },
        Ext4Error::NotADirectory => Error::NotADirectory { node },
        Ext4Error::IsADirectory => Error::IsADirectory { node },
        Ext4Error::IsASpecialFile => Error::NoSuchStream { node },
        Ext4Error::Encrypted => Error::Unsupported("encrypted ext4 file".into()),
        Ext4Error::Incompatible(i) => Error::Unsupported(format!("ext4: {i}").into()),
        Ext4Error::Corrupt(_) => other("damaged structure"),
        Ext4Error::Io(_) => other("read failed"),
        _ => other("path or file not usable"),
    }
}

/// The crate's positional reads, over a [`BlockSource`].
struct Reader(Arc<dyn BlockSource>);

impl Ext4Read for Reader {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        self.0.read_at(start_byte, dst).map_err(Box::new)?;
        Ok(())
    }
}

/// The handle every `Ext`, directory stream and open file shares.
struct Shared {
    jobs: Mutex<Sender<Job>>,
}

impl Shared {
    /// Runs `f` on the worker and returns its answer.
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut State) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = channel();
        let job: Job = Box::new(move |state| {
            let out = catch_unwind(AssertUnwindSafe(|| f(state)))
                .unwrap_or_else(|_| Err(other("the ext reader failed unexpectedly")));
            let _ = tx.send(out);
        });
        self.jobs
            .lock()
            .map_err(|_| other("the ext reader stopped"))?
            .send(job)
            .map_err(|_| other("the ext reader stopped"))?;
        rx.recv().map_err(|_| other("the ext reader stopped"))?
    }
}

/// A mounted ext2, ext3 or ext4 volume.
pub struct Ext {
    shared: Arc<Shared>,
    info: VolumeInfo,
    identity: Option<VolumeIdentity>,
}

/// Mounts the volume in `source`.
pub fn mount(source: Arc<dyn BlockSource>) -> Result<Ext> {
    let total = source.len();
    let (jobs, rx) = channel::<Job>();
    let (ready_tx, ready_rx) = channel();
    std::thread::Builder::new()
        .name("ext4-reader".into())
        .spawn(move || {
            let loaded = catch_unwind(AssertUnwindSafe(|| Ext4::load(Box::new(Reader(source)))));
            let fs = match loaded {
                Ok(Ok(fs)) => fs,
                Ok(Err(e)) => {
                    let _ = ready_tx.send(Err(map_err(e, 0)));
                    return;
                }
                Err(_) => {
                    let _ = ready_tx.send(Err(other("the ext reader failed unexpectedly")));
                    return;
                }
            };
            let label_bytes = *fs.label().as_bytes();
            let label_text: Vec<u8> = label_bytes
                .iter()
                .copied()
                .take_while(|&b| b != 0)
                .collect();
            let label = lossy(&label_text);
            let uuid = *fs.uuid().as_bytes();
            let mut state = State {
                fs,
                paths: vec![b"/".to_vec()],
                ids: HashMap::from([(b"/".to_vec(), 0)]),
                streams: HashMap::new(),
                next_stream: 0,
            };
            let _ = ready_tx.send(Ok((label, uuid)));
            while let Ok(job) = rx.recv() {
                job(&mut state);
            }
        })
        .map_err(|_| Error::Unsupported("starting the ext reader thread".into()))?;
    let (label, uuid) = ready_rx
        .recv()
        .map_err(|_| other("the ext reader stopped"))??;
    let mut info = VolumeInfo::new(if label.is_empty() {
        "ext volume".to_owned()
    } else {
        label
    });
    info.format = Some("ext");
    info.total_bytes = Some(total);
    Ok(Ext {
        shared: Arc::new(Shared {
            jobs: Mutex::new(jobs),
        }),
        info,
        identity: Some(VolumeIdentity {
            id: uuid,
            change: [0; 16],
        }),
    })
}

fn timestamp(t: ext4_view::Timestamp) -> Timestamp {
    Timestamp::from_unix(t.seconds(), t.nanoseconds())
}

fn kind_of(t: FileType) -> NodeKind {
    if t.is_dir() {
        NodeKind::Directory
    } else if t.is_regular_file() {
        NodeKind::File
    } else if t.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

fn link_bytes(fs: &Ext4, path: &[u8]) -> Result<Vec<u8>> {
    let target = fs.read_link(path).map_err(|e| map_err(e, 0))?;
    let bytes: &[u8] = target.as_ref();
    if bytes.len() > MAX_LINK_BYTES {
        return Err(limit("symbolic link target", MAX_LINK_BYTES as u64));
    }
    Ok(bytes.to_vec())
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// True when the symbolic link at `path` leads to a folder.
fn links_to_dir(fs: &Ext4, path: &[u8]) -> bool {
    fs.metadata(path).is_ok_and(|m| m.is_dir())
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut out = dir.to_vec();
    if out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(name);
    out
}

struct ExtDir {
    shared: Arc<Shared>,
    id: u64,
    done: bool,
}

impl Drop for ExtDir {
    fn drop(&mut self) {
        let id = self.id;
        let job: Job = Box::new(move |state| {
            state.streams.remove(&id);
        });
        if let Ok(jobs) = self.shared.jobs.lock() {
            let _ = jobs.send(job);
        }
    }
}

impl DirStream for ExtDir {
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
        if self.done {
            return Ok(DirProgress::Done);
        }
        if max == 0 {
            return Ok(DirProgress::More);
        }
        let id = self.id;
        let (rows, finished, failure) = self.shared.call(move |state| {
            let mut rows: Vec<Row> = Vec::new();
            let mut finished = false;
            let mut failure = None;
            while rows.len() < max {
                let Some(stream) = state.streams.get_mut(&id) else {
                    return Err(Error::InvalidCursor);
                };
                match stream.next() {
                    None => {
                        finished = true;
                        break;
                    }
                    Some(Err(e)) => {
                        failure = Some(map_err(e, 0));
                        break;
                    }
                    Some(Ok(entry)) => {
                        let name = entry.file_name();
                        let raw: &[u8] = name.as_ref();
                        if raw == b"." || raw == b".." {
                            continue;
                        }
                        let shown = lossy(raw);
                        let path = entry.path();
                        let path: &[u8] = path.as_ref();
                        let kind = entry.file_type().map_or(NodeKind::Other, kind_of);
                        let meta = entry
                            .metadata()
                            .ok()
                            .map(|m| (m.len(), timestamp(m.modified())));
                        let mut flags = EntryFlags::EMPTY;
                        if kind == NodeKind::Symlink && links_to_dir(&state.fs, path) {
                            flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
                        }
                        let node = state.intern(path)?;
                        rows.push((node, shown, kind, flags, meta));
                    }
                }
            }
            if finished || failure.is_some() {
                state.streams.remove(&id);
            }
            Ok((rows, finished, failure))
        })?;
        for (node, name, kind, flags, meta) in rows {
            out.push(DirEntry {
                node: NodeId(node),
                name: &name,
                kind,
                size: meta.filter(|_| kind != NodeKind::Directory).map(|m| m.0),
                resource_size: None,
                modified: meta.map(|m| m.1),
                flags,
            });
        }
        if let Some(e) = failure {
            self.done = true;
            return Err(e);
        }
        if finished {
            self.done = true;
            return Ok(DirProgress::Done);
        }
        Ok(DirProgress::More)
    }
}

/// The data of one file, read through the worker.
struct ExtFile {
    shared: Arc<Shared>,
    path: Vec<u8>,
    len: u64,
}

impl BlockSource for ExtFile {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let path = self.path.clone();
        let want = buf.len();
        let bytes = self.shared.call(move |state| {
            let mut file = state.fs.open(path.as_slice()).map_err(|e| map_err(e, 0))?;
            file.seek_to(offset).map_err(|e| map_err(e, 0))?;
            let mut data = vec![0u8; want];
            let mut filled = 0usize;
            while filled < want {
                let Some(rest) = data.get_mut(filled..) else {
                    break;
                };
                let n = file.read_bytes(rest).map_err(|e| map_err(e, 0))?;
                if n == 0 {
                    return Err(Corruption::new(
                        STRUCTURE,
                        Location::Byte(offset),
                        CorruptKind::Truncated {
                            needed: want as u64,
                            available: filled as u64,
                        },
                    )
                    .into());
                }
                filled = filled.saturating_add(n);
            }
            Ok(data)
        })?;
        buf.copy_from_slice(&bytes);
        Ok(())
    }
}

impl FileSystem for Ext {
    fn info(&self) -> VolumeInfo {
        self.info.clone()
    }

    fn root(&self) -> NodeId {
        NodeId(0)
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata> {
        let n = node.0;
        self.shared.call(move |state| {
            let path = state.path(n)?;
            let m = state
                .fs
                .symlink_metadata(path.as_slice())
                .map_err(|e| map_err(e, n))?;
            let kind = kind_of(m.file_type());
            let link_target = if kind == NodeKind::Symlink {
                link_bytes(&state.fs, &path).ok().map(|b| lossy(&b))
            } else {
                None
            };
            let mut flags = EntryFlags::EMPTY;
            if kind == NodeKind::Symlink && links_to_dir(&state.fs, &path) {
                flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
            }
            let ty: u16 = match kind {
                NodeKind::Directory => 0o040000,
                NodeKind::File => 0o100000,
                NodeKind::Symlink => 0o120000,
                _ => 0,
            };
            Ok(Metadata {
                node,
                kind,
                size: (kind != NodeKind::Directory).then(|| m.len()),
                resource_size: None,
                created: m.created().map(timestamp),
                modified: Some(timestamp(m.modified())),
                accessed: Some(timestamp(m.accessed())),
                flags,
                link_target,
                finder_codes: None,
                owner: Some(Owner {
                    user: m.uid(),
                    group: m.gid(),
                    mode: ty | m.mode(),
                }),
                attributes: None,
                finder_info: None,
            })
        })
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
        let n = dir.0;
        let name = name.as_bytes().to_vec();
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            return Ok(None);
        }
        self.shared.call(move |state| {
            let parent = state.path(n)?;
            let is_dir = state
                .fs
                .metadata(parent.as_slice())
                .map_err(|e| map_err(e, n))?
                .is_dir();
            if !is_dir {
                return Err(Error::NotADirectory { node: n });
            }
            let path = join(&parent, &name);
            match state.fs.symlink_metadata(path.as_slice()) {
                Ok(_) => Ok(Some(NodeId(state.intern(&path)?))),
                Err(Ext4Error::NotFound) => Ok(None),
                Err(e) => Err(map_err(e, n)),
            }
        })
    }

    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
        let n = dir.0;
        let id = self.shared.call(move |state| {
            let path = state.path(n)?;
            let stream = state
                .fs
                .read_dir(path.as_slice())
                .map_err(|e| map_err(e, n))?;
            let id = state.next_stream;
            state.next_stream = id.wrapping_add(1);
            state.streams.insert(id, stream);
            Ok(id)
        })?;
        Ok(Box::new(ExtDir {
            shared: Arc::clone(&self.shared),
            id,
            done: false,
        }))
    }

    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        let n = node.0;
        if !matches!(stream, Stream::Data) {
            return Err(Error::NoSuchStream { node: n });
        }
        let shared = Arc::clone(&self.shared);
        self.shared.call(move |state| {
            let path = state.path(n)?;
            let m = state
                .fs
                .symlink_metadata(path.as_slice())
                .map_err(|e| map_err(e, n))?;
            let source: Arc<dyn BlockSource> = if m.is_dir() {
                return Err(Error::IsADirectory { node: n });
            } else if m.is_symlink() {
                Arc::new(MemSource::new(link_bytes(&state.fs, &path)?))
            } else if m.file_type().is_regular_file() {
                Arc::new(ExtFile {
                    shared,
                    path,
                    len: m.len(),
                })
            } else {
                return Err(Error::NoSuchStream { node: n });
            };
            Ok(source)
        })
    }

    fn identity(&self) -> Option<VolumeIdentity> {
        self.identity
    }
}
