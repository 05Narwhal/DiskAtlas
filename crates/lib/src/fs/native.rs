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

//! Host folders through `std::fs`.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{DirEntry as StdDirEntry, FileType, Metadata as StdMetadata, ReadDir};
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};

use crate::error::{CorruptKind, Corruption, Error, IoContext, Location, Result};
use crate::fs::{
    DirBatch, DirEntry, DirProgress, DirStream, EntryFlags, FileSystem, MAX_SCAN_FOLDERS, Metadata,
    NodeId, NodeKind, ScanEntry, ScanSummary, Stream, Timestamp, VolumeInfo,
};
use crate::io::block::{BlockSource, MemSource};
use crate::io::file::FileSource;

/// The id of the top folder. `NodeId(0)` is never handed out.
const ROOT: NodeId = NodeId(1);

/// The first id [`FileSystem::scan`] on a [`NativeFs`] hands out; each
/// scan counts up from here. Scan ids are never interned: they are valid
/// only as `node` and `parent` links between the entries of that one scan.
/// Listing, opening, `metadata` and `lookup` answer `Error::NotFound` for
/// them; the defaulted methods (`xattrs`, `read_xattr`, `extents`) answer
/// as for any node without such data and never panic. To reach a scanned
/// entry, walk its names from the root with [`FileSystem::lookup`].
pub const FIRST_SCAN_ID: NodeId = NodeId(1 << 63);

/// One interned node: where it sits and what it is called.
struct Node {
    parent: NodeId,
    name: Arc<OsStr>,
}

/// The interning table. Index `i` of `nodes` holds the node with id `i + 1`.
struct Table {
    nodes: Vec<Node>,
    /// Parent id -> child name -> child id. The name is the same `Arc` the
    /// node holds, so every name is stored once.
    children: HashMap<NodeId, HashMap<Arc<OsStr>, NodeId>>,
}

impl Table {
    fn new() -> Self {
        let root = Node {
            parent: NodeId(0),
            name: Arc::from(OsStr::new("")),
        };
        Self {
            nodes: vec![root],
            children: HashMap::new(),
        }
    }

    fn get(&self, id: NodeId) -> Option<&Node> {
        let index = usize::try_from(id.0.checked_sub(1)?).ok()?;
        self.nodes.get(index)
    }

    fn find(&self, parent: NodeId, name: &OsStr) -> Option<NodeId> {
        self.children.get(&parent)?.get(name).copied()
    }

    /// The id of `name` under `parent`, created when it is new.
    fn intern(&mut self, parent: NodeId, name: &OsStr) -> NodeId {
        if let Some(id) = self.find(parent, name) {
            return id;
        }
        let id = NodeId(
            u64::try_from(self.nodes.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        );
        let name: Arc<OsStr> = Arc::from(name);
        self.nodes.push(Node {
            parent,
            name: Arc::clone(&name),
        });
        self.children.entry(parent).or_default().insert(name, id);
        id
    }
}

struct Inner {
    name: String,
    root_path: PathBuf,
    table: RwLock<Table>,
}

impl Inner {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Table> {
        self.table.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Table> {
        self.table.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn path(&self, node: NodeId) -> Option<PathBuf> {
        let mut names = Vec::new();
        {
            let table = self.read();
            // A parent chain longer than the table would be a loop.
            let limit = table.nodes.len();
            let mut current = node;
            let mut steps = 0_usize;
            while current != ROOT {
                let entry = table.get(current)?;
                names.push(Arc::clone(&entry.name));
                current = entry.parent;
                steps = steps.saturating_add(1);
                if steps > limit {
                    return None;
                }
            }
        }
        let mut path = self.root_path.clone();
        for name in names.iter().rev() {
            path.push(name.as_ref());
        }
        Some(path)
    }

    fn intern(&self, parent: NodeId, name: &OsStr) -> NodeId {
        if let Some(id) = self.read().find(parent, name) {
            return id;
        }
        self.write().intern(parent, name)
    }
}

/// A host folder and everything below it, read through `std::fs`.
///
/// Node ids from `lookup` and `read_dir` are interned (parent, name) pairs:
/// listing a folder twice gives the same ids. The table only grows for as
/// long as the filesystem lives, so it holds what was browsed (bounding it
/// is a later task). [`FileSystem::scan`] walks the host itself and reports
/// scan-local ids from [`FIRST_SCAN_ID`] up, so indexing the whole tree
/// adds nothing to the table.
#[derive(Clone)]
pub struct NativeFs {
    inner: Arc<Inner>,
}

impl NativeFs {
    /// `root` becomes the top folder; `name` is what [`FileSystem::info`]
    /// reports as the volume name.
    ///
    /// # Errors
    ///
    /// Fails when `root` cannot be read as a folder.
    pub fn new(root: impl Into<PathBuf>, name: impl Into<String>) -> Result<Self> {
        let root_path: PathBuf = root.into();
        let opening = |source: io::Error| {
            Error::io(
                IoContext::Path {
                    op: "opening",
                    path: root_path.clone(),
                },
                source,
            )
        };
        let meta = std::fs::metadata(&root_path).map_err(opening)?;
        if !meta.is_dir() {
            return Err(Error::NotADirectory { node: ROOT.0 });
        }
        drop(std::fs::read_dir(&root_path).map_err(opening)?);
        Ok(Self {
            inner: Arc::new(Inner {
                name: name.into(),
                root_path,
                table: RwLock::new(Table::new()),
            }),
        })
    }

    /// The host path of a node this filesystem has handed out. `None` for an
    /// unknown id.
    pub fn path(&self, node: NodeId) -> Option<PathBuf> {
        self.inner.path(node)
    }
}

fn kind_of(file_type: FileType) -> NodeKind {
    if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_file() {
        NodeKind::File
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

fn size_of(kind: NodeKind, meta: &StdMetadata) -> Option<u64> {
    match kind {
        NodeKind::File | NodeKind::Symlink => Some(meta.len()),
        _ => None,
    }
}

#[cfg(windows)]
fn is_hidden(_name: &OsStr, meta: Option<&StdMetadata>) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.is_some_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

#[cfg(not(windows))]
fn is_hidden(name: &OsStr, _meta: Option<&StdMetadata>) -> bool {
    name.as_encoded_bytes().starts_with(b".")
}

#[cfg(windows)]
fn links_to_directory(file_type: FileType, _path: &Path) -> bool {
    use std::os::windows::fs::FileTypeExt;
    file_type.is_symlink_dir()
}

#[cfg(not(windows))]
fn links_to_directory(_file_type: FileType, path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

/// The kind and flags of an entry. `path` is only built when the entry is a
/// symbolic link.
fn classify(
    name: &OsStr,
    file_type: Option<FileType>,
    meta: Option<&StdMetadata>,
    path: impl FnOnce() -> PathBuf,
) -> (NodeKind, EntryFlags) {
    let kind = file_type.map_or(NodeKind::Other, kind_of);
    let mut flags = EntryFlags::EMPTY;
    if is_hidden(name, meta) {
        flags = flags.with(EntryFlags::HIDDEN);
    }
    if let (NodeKind::Symlink, Some(file_type)) = (kind, file_type)
        && links_to_directory(file_type, &path())
    {
        flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
    }
    (kind, flags)
}

/// Names that could leave the folder they are looked up in.
fn is_unsafe_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return true;
    }
    // `:` is a drive prefix or a stream separator on Windows, and `join`
    // would let a drive prefix replace the whole path.
    let separators: &[char] = if cfg!(windows) {
        &['/', '\\', '\0', ':']
    } else {
        &['/', '\\', '\0']
    };
    name.contains(separators)
}

fn timestamp(time: io::Result<std::time::SystemTime>) -> Option<Timestamp> {
    time.ok().map(Timestamp::from_system_time)
}

impl FileSystem for NativeFs {
    fn info(&self) -> VolumeInfo {
        VolumeInfo {
            name: self.inner.name.clone(),
            format: None,
            total_bytes: None,
            free_bytes: None,
            resource_forks: false,
            journal: None,
            local_times: false,
        }
    }

    fn root(&self) -> NodeId {
        ROOT
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata> {
        let path = self
            .inner
            .path(node)
            .ok_or(Error::NotFound { node: node.0 })?;
        let reading = |source: io::Error| {
            Error::io(
                IoContext::Path {
                    op: "reading",
                    path: path.clone(),
                },
                source,
            )
        };
        let meta = std::fs::symlink_metadata(&path).map_err(reading)?;
        let file_type = meta.file_type();
        let name = path.file_name().unwrap_or_default();
        let (kind, flags) = classify(name, Some(file_type), Some(&meta), || path.clone());
        let link_target = if kind == NodeKind::Symlink {
            std::fs::read_link(&path)
                .ok()
                .map(|target| target.to_string_lossy().into_owned())
        } else {
            None
        };
        Ok(Metadata {
            node,
            kind,
            size: size_of(kind, &meta),
            resource_size: None,
            created: timestamp(meta.created()),
            modified: timestamp(meta.modified()),
            accessed: timestamp(meta.accessed()),
            flags,
            link_target,
            finder_codes: None,
            owner: None,
            attributes: None,
            finder_info: None,
        })
    }

    /// The data of a regular file. A symbolic link gives its stored target
    /// path (it is not followed). The host has no resource forks here, so
    /// `Stream::Resource` is always `Error::NoSuchStream`.
    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        let path = self
            .inner
            .path(node)
            .ok_or(Error::NotFound { node: node.0 })?;
        let reading = |source: io::Error| {
            Error::io(
                IoContext::Path {
                    op: "reading",
                    path: path.clone(),
                },
                source,
            )
        };
        let file_type = std::fs::symlink_metadata(&path)
            .map_err(reading)?
            .file_type();
        if file_type.is_dir() {
            return Err(Error::IsADirectory { node: node.0 });
        }
        match stream {
            Stream::Resource => Err(Error::NoSuchStream { node: node.0 }),
            Stream::Data if file_type.is_symlink() => {
                let target = std::fs::read_link(&path).map_err(reading)?;
                let bytes = target.to_string_lossy().into_owned().into_bytes();
                Ok(Arc::new(MemSource::new(bytes)))
            }
            Stream::Data if file_type.is_file() => Ok(Arc::new(FileSource::open(&path)?)),
            Stream::Data => Err(Error::Unsupported(
                "opening a device, pipe or socket".into(),
            )),
        }
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
        if is_unsafe_name(name) {
            return Ok(None);
        }
        let dir_path = self
            .inner
            .path(dir)
            .ok_or(Error::NotFound { node: dir.0 })?;
        let path = dir_path.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(Some(self.inner.intern(dir, OsStr::new(name)))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(
                IoContext::Path {
                    op: "reading",
                    path,
                },
                e,
            )),
        }
    }

    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
        let path = self
            .inner
            .path(dir)
            .ok_or(Error::NotFound { node: dir.0 })?;
        match std::fs::read_dir(&path) {
            Ok(iter) => Ok(Box::new(NativeDirStream {
                inner: Arc::clone(&self.inner),
                dir,
                path,
                iter: Some(iter),
            })),
            Err(source) => {
                if std::fs::metadata(&path).is_ok_and(|m| !m.is_dir()) {
                    Err(Error::NotADirectory { node: dir.0 })
                } else {
                    Err(Error::io(
                        IoContext::Path {
                            op: "listing",
                            path,
                        },
                        source,
                    ))
                }
            }
        }
    }

    /// Walks the host breadth-first with `std::fs::read_dir`, without the
    /// intern table. Ids are scan-local (see [`FIRST_SCAN_ID`]); the root's
    /// children have `parent == root()`.
    ///
    /// Otherwise as [`default_scan`](crate::fs::default_scan): symbolic
    /// links and junctions are reported and never followed; a folder that
    /// cannot be listed, or whose listing fails part way, counts as one
    /// skipped structure (entries already read are still visited); fails with
    /// `Corrupt(LimitExceeded)` past [`MAX_SCAN_FOLDERS`] queued folders.
    /// Names are reported as listings report them (lossy UTF-8); folders are
    /// still entered through their real host names.
    fn scan(&self, visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>) -> Result<ScanSummary> {
        scan_tree(&self.inner.root_path, visit)
    }
}

fn too_many_folders() -> Error {
    Corruption::new(
        "folder tree",
        Location::Unspecified,
        CorruptKind::LimitExceeded {
            what: "folders queued by the scan",
            limit: MAX_SCAN_FOLDERS,
        },
    )
    .into()
}

fn too_many_entries() -> Error {
    Corruption::new(
        "folder tree",
        Location::Unspecified,
        CorruptKind::LimitExceeded {
            what: "ids handed out by the scan",
            limit: u64::MAX - FIRST_SCAN_ID.0,
        },
    )
    .into()
}

/// [`NativeFs::scan`]: `root` is the host folder of [`ROOT`].
fn scan_tree(
    root: &Path,
    visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
) -> Result<ScanSummary> {
    let mut summary = ScanSummary::default();
    // Folders still to list: their scan id and host path. Only folders wait
    // here, and only until they are listed.
    let mut queue: VecDeque<(NodeId, PathBuf)> = VecDeque::new();
    queue.push_back((ROOT, root.to_path_buf()));
    let mut folders: u64 = 1;
    let mut next_id = FIRST_SCAN_ID.0;

    while let Some((dir, path)) = queue.pop_front() {
        let Ok(listing) = std::fs::read_dir(&path) else {
            summary.skipped = summary.skipped.saturating_add(1);
            continue;
        };
        for item in listing {
            let Ok(entry) = item else {
                summary.skipped = summary.skipped.saturating_add(1);
                break;
            };
            let g = gather(&entry);
            let node = NodeId(next_id);
            next_id = next_id.checked_add(1).ok_or_else(too_many_entries)?;
            summary.entries = summary.entries.saturating_add(1);
            let flow = visit(ScanEntry {
                node,
                parent: dir,
                name: &g.name.to_string_lossy(),
                kind: g.kind,
                size: g.size,
                resource_size: None,
                modified: g.modified,
                flags: g.flags,
                finder_type: None,
            });
            if flow.is_break() {
                return Ok(summary);
            }
            // `kind` comes from the entry's own type, which does not follow
            // links: a link or junction to a folder is `Symlink`, never
            // `Directory`, so it is never entered.
            if g.kind == NodeKind::Directory {
                if folders >= MAX_SCAN_FOLDERS {
                    return Err(too_many_folders());
                }
                folders = folders.saturating_add(1);
                queue.push_back((node, entry.path()));
            }
        }
    }
    Ok(summary)
}

/// What one listing call learns about an entry before it has an id.
struct Gathered {
    name: OsString,
    kind: NodeKind,
    size: Option<u64>,
    modified: Option<Timestamp>,
    flags: EntryFlags,
}

fn gather(entry: &StdDirEntry) -> Gathered {
    let name = entry.file_name();
    // Does not follow links; free on Windows, one stat elsewhere.
    let meta = entry.metadata().ok();
    let file_type = entry
        .file_type()
        .ok()
        .or_else(|| meta.as_ref().map(StdMetadata::file_type));
    let (kind, flags) = classify(&name, file_type, meta.as_ref(), || entry.path());
    Gathered {
        size: meta.as_ref().and_then(|m| size_of(kind, m)),
        modified: meta.as_ref().and_then(|m| timestamp(m.modified())),
        name,
        kind,
        flags,
    }
}

struct NativeDirStream {
    inner: Arc<Inner>,
    dir: NodeId,
    path: PathBuf,
    /// `None` once the listing has ended or failed.
    iter: Option<ReadDir>,
}

impl DirStream for NativeDirStream {
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
        let Some(iter) = self.iter.as_mut() else {
            return Ok(DirProgress::Done);
        };
        if max == 0 {
            return Ok(DirProgress::More);
        }

        // All the I/O happens here, with no lock held.
        let mut gathered = Vec::with_capacity(max.min(256));
        let mut ended = false;
        let mut failure = None;
        while gathered.len() < max {
            match iter.next() {
                None => {
                    ended = true;
                    break;
                }
                Some(Ok(entry)) => gathered.push(gather(&entry)),
                Some(Err(source)) => {
                    failure = Some(source);
                    break;
                }
            }
        }

        if !gathered.is_empty() {
            let ids: Vec<NodeId> = {
                let mut table = self.inner.write();
                gathered
                    .iter()
                    .map(|g| table.intern(self.dir, &g.name))
                    .collect()
            };
            for (g, node) in gathered.iter().zip(ids) {
                out.push(DirEntry {
                    node,
                    name: &g.name.to_string_lossy(),
                    kind: g.kind,
                    size: g.size,
                    resource_size: None,
                    modified: g.modified,
                    flags: g.flags,
                });
            }
        }

        if let Some(source) = failure {
            self.iter = None;
            return Err(Error::io(
                IoContext::Path {
                    op: "listing",
                    path: self.path.clone(),
                },
                source,
            ));
        }
        if ended {
            self.iter = None;
            return Ok(DirProgress::Done);
        }
        Ok(DirProgress::More)
    }
}

/// A place on this computer worth listing in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRoot {
    /// The folder to open.
    pub path: PathBuf,
    /// What to call it.
    pub label: String,
    /// What sort of place it is.
    pub kind: HostRootKind,
}

/// What sort of place a [`HostRoot`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRootKind {
    /// The user's home folder.
    Home,
    /// A drive or mounted volume.
    Volume,
}

/// The user's home folder first (labelled "Home"), then the volumes.
///
/// Blocking, and may take a while when a drive is slow to answer. Never
/// fails: what cannot be probed is left out.
pub fn host_roots() -> Vec<HostRoot> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::home_dir().filter(|p| p.is_dir()) {
        roots.push(HostRoot {
            path: home,
            label: String::from("Home"),
            kind: HostRootKind::Home,
        });
    }
    push_volumes(&mut roots);
    roots
}

#[cfg(windows)]
fn push_volumes(roots: &mut Vec<HostRoot>) {
    for letter in 'A'..='Z' {
        let path = PathBuf::from(format!("{letter}:\\"));
        if std::fs::metadata(&path).is_ok() {
            roots.push(HostRoot {
                path,
                label: format!("{letter}:"),
                kind: HostRootKind::Volume,
            });
        }
    }
}

/// The real sub-folders of `dir` (symbolic links excluded), by name.
#[cfg(not(windows))]
fn push_subfolders(roots: &mut Vec<HostRoot>, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<HostRoot> = entries
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| HostRoot {
            path: e.path(),
            label: e.file_name().to_string_lossy().into_owned(),
            kind: HostRootKind::Volume,
        })
        .collect();
    found.sort_by(|a, b| a.label.cmp(&b.label));
    roots.extend(found);
}

#[cfg(not(windows))]
fn push_root_folder(roots: &mut Vec<HostRoot>) {
    roots.push(HostRoot {
        path: PathBuf::from("/"),
        label: String::from("/"),
        kind: HostRootKind::Volume,
    });
}

#[cfg(target_os = "macos")]
fn push_volumes(roots: &mut Vec<HostRoot>) {
    push_root_folder(roots);
    push_subfolders(roots, Path::new("/Volumes"));
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn push_volumes(roots: &mut Vec<HostRoot>) {
    push_root_folder(roots);
    if let Some(user) = std::env::var_os("USER") {
        push_subfolders(roots, &Path::new("/media").join(&user));
        push_subfolders(roots, &Path::new("/run/media").join(&user));
    }
    push_subfolders(roots, Path::new("/mnt"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A uniquely named temp folder, removed (best effort) on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("disk-atlas-native-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Three files (3, 0 and 10 bytes) and two folders.
    fn fixture() -> (TempDir, NativeFs) {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("a.txt"), b"abc").unwrap();
        std::fs::write(dir.0.join("b.bin"), b"").unwrap();
        std::fs::write(dir.0.join("c.log"), b"0123456789").unwrap();
        std::fs::create_dir(dir.0.join("sub1")).unwrap();
        std::fs::create_dir(dir.0.join("sub2")).unwrap();
        std::fs::write(dir.0.join("sub1").join("inner.txt"), b"x").unwrap();
        let fs = NativeFs::new(&dir.0, "Fixture").unwrap();
        (dir, fs)
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Owned {
        node: NodeId,
        name: String,
        kind: NodeKind,
        size: Option<u64>,
        modified: Option<Timestamp>,
        flags: EntryFlags,
    }

    /// Lists `dir` to the end with pages of `max`, sorted by name.
    fn list(fs: &NativeFs, dir: NodeId, max: usize) -> Vec<Owned> {
        let mut stream = fs.read_dir(dir).unwrap();
        let mut all = Vec::new();
        for _ in 0..1000 {
            let mut batch = DirBatch::new();
            let progress = stream.next_batch(&mut batch, max).unwrap();
            assert!(batch.len() <= max);
            all.extend(batch.iter().map(|e| Owned {
                node: e.node,
                name: e.name.to_owned(),
                kind: e.kind,
                size: e.size,
                modified: e.modified,
                flags: e.flags,
            }));
            if progress == DirProgress::Done {
                all.sort_by(|a, b| a.name.cmp(&b.name));
                return all;
            }
        }
        panic!("listing did not finish");
    }

    fn expect_err<T>(result: Result<T>) -> Error {
        match result {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn lists_files_and_folders() {
        let (_dir, fs) = fixture();
        let entries = list(&fs, fs.root(), 100);
        let summary: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.size))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("a.txt", NodeKind::File, Some(3)),
                ("b.bin", NodeKind::File, Some(0)),
                ("c.log", NodeKind::File, Some(10)),
                ("sub1", NodeKind::Directory, None),
                ("sub2", NodeKind::Directory, None),
            ]
        );
        assert!(entries.iter().all(|e| e.modified.is_some()));
        assert!(
            entries
                .iter()
                .all(|e| !e.flags.contains(EntryFlags::HIDDEN))
        );
        assert_eq!(fs.info().name, "Fixture");
        assert_eq!(fs.info().format, None);
    }

    #[test]
    fn paging_returns_every_entry_once() {
        let (_dir, fs) = fixture();
        for max in [1, 2, 3, 5, 6] {
            let entries = list(&fs, fs.root(), max);
            let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(
                names,
                ["a.txt", "b.bin", "c.log", "sub1", "sub2"],
                "max {max}"
            );
        }
    }

    #[test]
    fn max_zero_appends_nothing_and_returns_more() {
        let (_dir, fs) = fixture();
        let mut stream = fs.read_dir(fs.root()).unwrap();
        let mut batch = DirBatch::new();
        assert_eq!(stream.next_batch(&mut batch, 0).unwrap(), DirProgress::More);
        assert!(batch.is_empty());
        // The stream still lists everything afterwards.
        stream.next_batch(&mut batch, 100).unwrap();
        assert_eq!(batch.len(), 5);
    }

    #[test]
    fn finished_stream_keeps_returning_done() {
        let (_dir, fs) = fixture();
        let mut stream = fs.read_dir(fs.root()).unwrap();
        let mut batch = DirBatch::new();
        while stream.next_batch(&mut batch, 2).unwrap() == DirProgress::More {}
        let len = batch.len();
        assert_eq!(stream.next_batch(&mut batch, 2).unwrap(), DirProgress::Done);
        assert_eq!(batch.len(), len);
    }

    #[test]
    fn same_folder_same_ids_and_paths_round_trip() {
        let (dir, fs) = fixture();
        let first = list(&fs, fs.root(), 2);
        let second = list(&fs, fs.root(), 3);
        assert_eq!(first, second);
        assert_eq!(fs.path(fs.root()), Some(dir.0.clone()));
        assert!(
            first
                .iter()
                .all(|e| e.node != NodeId(0) && e.node != fs.root())
        );
        for entry in &first {
            assert_eq!(fs.path(entry.node), Some(dir.0.join(&entry.name)));
        }
        // A nested node resolves through its parent chain.
        let sub1 = first.iter().find(|e| e.name == "sub1").unwrap().node;
        let inner = list(&fs, sub1, 10);
        assert_eq!(
            fs.path(inner[0].node),
            Some(dir.0.join("sub1").join("inner.txt"))
        );
        assert_eq!(fs.path(NodeId(0)), None);
        assert_eq!(fs.path(NodeId(9999)), None);
    }

    #[test]
    fn lookup_finds_and_rejects() {
        let (_dir, fs) = fixture();
        let root = fs.root();
        let listed = list(&fs, root, 10);
        let found = fs.lookup(root, "a.txt").unwrap().unwrap();
        assert_eq!(
            found,
            listed.iter().find(|e| e.name == "a.txt").unwrap().node
        );
        assert_eq!(fs.lookup(root, "missing").unwrap(), None);
        for bad in ["..", ".", "", "a/b", "a\\b", "a\0b"] {
            assert_eq!(fs.lookup(root, bad).unwrap(), None, "{bad:?}");
        }
        assert!(matches!(
            fs.lookup(NodeId(9999), "a.txt"),
            Err(Error::NotFound { node: 9999 })
        ));
    }

    #[test]
    fn metadata_of_file_and_folder() {
        let (_dir, fs) = fixture();
        let file = fs.lookup(fs.root(), "c.log").unwrap().unwrap();
        let meta = fs.metadata(file).unwrap();
        assert_eq!(meta.node, file);
        assert_eq!(meta.kind, NodeKind::File);
        assert_eq!(meta.size, Some(10));
        assert!(meta.modified.is_some());
        assert_eq!(meta.link_target, None);
        assert_eq!(meta.resource_size, None);
        assert_eq!(meta.finder_codes, None);
        assert_eq!(meta.owner, None);
        assert_eq!(meta.attributes, None);

        let folder = fs.lookup(fs.root(), "sub1").unwrap().unwrap();
        let meta = fs.metadata(folder).unwrap();
        assert_eq!(meta.kind, NodeKind::Directory);
        assert_eq!(meta.size, None);

        let root = fs.metadata(fs.root()).unwrap();
        assert_eq!(root.kind, NodeKind::Directory);

        assert!(matches!(
            fs.metadata(NodeId(0)),
            Err(Error::NotFound { node: 0 })
        ));
        assert!(matches!(
            fs.metadata(NodeId(4242)),
            Err(Error::NotFound { node: 4242 })
        ));
    }

    fn read_all(src: &dyn BlockSource) -> Vec<u8> {
        let mut buf = vec![0u8; usize::try_from(src.len()).unwrap()];
        src.read_at(0, &mut buf).unwrap();
        buf
    }

    #[test]
    fn open_reads_a_regular_file() {
        let (_dir, fs) = fixture();
        let node = |name: &str| fs.lookup(fs.root(), name).unwrap().unwrap();
        assert_eq!(
            read_all(&*fs.open(node("c.log"), Stream::Data).unwrap()),
            b"0123456789"
        );
        assert_eq!(
            read_all(&*fs.open(node("b.bin"), Stream::Data).unwrap()),
            b""
        );
    }

    #[test]
    fn open_failures() {
        let (dir, fs) = fixture();
        let node = |name: &str| fs.lookup(fs.root(), name).unwrap().unwrap();
        for stream in [Stream::Data, Stream::Resource] {
            assert!(matches!(
                fs.open(node("sub1"), stream),
                Err(Error::IsADirectory { .. })
            ));
            assert!(matches!(
                fs.open(NodeId(9999), stream),
                Err(Error::NotFound { node: 9999 })
            ));
        }
        // The host has no resource forks.
        assert!(matches!(
            fs.open(node("a.txt"), Stream::Resource),
            Err(Error::NoSuchStream { .. })
        ));
        // A file removed after it was listed is an error, not a panic.
        let gone = node("a.txt");
        std::fs::remove_file(dir.0.join("a.txt")).unwrap();
        assert!(matches!(fs.open(gone, Stream::Data), Err(Error::Io { .. })));
        // The trait defaults stay: no attributes on the host.
        assert!(fs.xattrs(node("c.log")).unwrap().is_empty());
        assert_eq!(fs.read_xattr(node("c.log"), "user.a", 8).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn open_gives_the_target_path_of_a_symlink() {
        let (dir, fs) = fixture();
        std::os::unix::fs::symlink("a.txt", dir.0.join("link")).unwrap();
        let link = fs.lookup(fs.root(), "link").unwrap().unwrap();
        // The stored path, not the content of what it points at.
        assert_eq!(read_all(&*fs.open(link, Stream::Data).unwrap()), b"a.txt");
        assert!(matches!(
            fs.open(link, Stream::Resource),
            Err(Error::NoSuchStream { .. })
        ));
    }

    #[test]
    fn info_reports_no_resource_forks_and_no_journal() {
        let (_dir, fs) = fixture();
        let info = fs.info();
        assert!(!info.resource_forks);
        assert_eq!(info.journal, None);
    }

    #[test]
    fn read_dir_errors() {
        let (dir, fs) = fixture();
        let file = fs.lookup(fs.root(), "a.txt").unwrap().unwrap();
        assert!(matches!(
            expect_err(fs.read_dir(file)),
            Error::NotADirectory { .. }
        ));
        assert!(matches!(
            expect_err(fs.read_dir(NodeId(777))),
            Error::NotFound { node: 777 }
        ));
        let missing = dir.0.join("does-not-exist");
        assert!(NativeFs::new(&missing, "x").is_err());
        assert!(matches!(
            NativeFs::new(dir.0.join("a.txt"), "x"),
            Err(Error::NotADirectory { .. })
        ));
    }

    #[test]
    fn folder_deleted_after_listing_is_an_error_not_a_panic() {
        let (dir, fs) = fixture();
        let sub = fs.lookup(fs.root(), "sub2").unwrap().unwrap();
        std::fs::remove_dir(dir.0.join("sub2")).unwrap();
        assert!(fs.read_dir(sub).is_err());
        assert!(fs.metadata(sub).is_err());
        assert_eq!(fs.lookup(sub, "anything").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_folder() {
        let (dir, fs) = fixture();
        std::os::unix::fs::symlink(dir.0.join("sub1"), dir.0.join("link")).unwrap();
        let entries = list(&fs, fs.root(), 10);
        let link = entries.iter().find(|e| e.name == "link").unwrap();
        assert_eq!(link.kind, NodeKind::Symlink);
        assert!(link.flags.contains(EntryFlags::LINKS_TO_DIRECTORY));

        let mut batch = DirBatch::new();
        batch.push(DirEntry {
            node: link.node,
            name: &link.name,
            kind: link.kind,
            size: link.size,
            resource_size: None,
            modified: link.modified,
            flags: link.flags,
        });
        assert!(batch.get(0).unwrap().is_browsable());

        let target = list(&fs, link.node, 10);
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].name, "inner.txt");

        let meta = fs.metadata(link.node).unwrap();
        assert_eq!(meta.kind, NodeKind::Symlink);
        assert_eq!(
            meta.link_target.as_deref(),
            Some(dir.0.join("sub1").to_string_lossy().as_ref())
        );
        assert!(meta.flags.contains(EntryFlags::LINKS_TO_DIRECTORY));
    }

    #[test]
    fn host_roots_are_existing_folders() {
        let roots = host_roots();
        assert!(!roots.is_empty());
        for root in &roots {
            assert!(root.path.is_dir(), "{}", root.path.display());
            assert!(!root.label.is_empty());
        }
        if let Some(first) = roots.first()
            && first.kind == HostRootKind::Home
        {
            assert_eq!(first.label, "Home");
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Scanned {
        node: NodeId,
        parent: NodeId,
        name: String,
        kind: NodeKind,
        size: Option<u64>,
        modified: Option<Timestamp>,
        flags: EntryFlags,
    }

    /// Scans `fs` to the end (or `stop_after` entries), in arrival order.
    fn scan_all(fs: &NativeFs, stop_after: usize) -> (Vec<Scanned>, ScanSummary) {
        let mut seen = Vec::new();
        let summary = fs
            .scan(&mut |e| {
                seen.push(Scanned {
                    node: e.node,
                    parent: e.parent,
                    name: e.name.to_owned(),
                    kind: e.kind,
                    size: e.size,
                    modified: e.modified,
                    flags: e.flags,
                });
                if seen.len() >= stop_after {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        (seen, summary)
    }

    fn table_len(fs: &NativeFs) -> usize {
        fs.inner.read().nodes.len()
    }

    #[test]
    fn scan_reports_every_entry_with_scan_local_ids() {
        let (_dir, fs) = fixture();
        let (seen, summary) = scan_all(&fs, usize::MAX);
        assert_eq!(
            summary,
            ScanSummary {
                entries: 6,
                skipped: 0
            }
        );
        assert_eq!(seen.len(), 6);

        let mut ids: Vec<u64> = seen.iter().map(|e| e.node.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 6, "ids are unique");
        assert!(ids.iter().all(|&id| id >= FIRST_SCAN_ID.0));

        let by_name = |name: &str| seen.iter().find(|e| e.name == name).unwrap();
        let sub1 = by_name("sub1");
        let inner = by_name("inner.txt");
        assert_eq!(inner.parent, sub1.node);
        assert_eq!(inner.kind, NodeKind::File);
        assert_eq!(inner.size, Some(1));
        // Breadth-first: a folder is reported before what is in it.
        let at = |name: &str| seen.iter().position(|e| e.name == name).unwrap();
        assert!(at("sub1") < at("inner.txt"));

        // The root's children carry what a listing of the root reports,
        // apart from the node id. (A folder's time may move between the two
        // reads: NTFS updates the parent's copy of it lazily.)
        let mut top: Vec<_> = seen.iter().filter(|e| e.parent == fs.root()).collect();
        top.sort_by(|a, b| a.name.cmp(&b.name));
        let listed = list(&fs, fs.root(), 100);
        assert_eq!(top.len(), listed.len());
        for (s, l) in top.iter().zip(&listed) {
            assert_eq!(
                (&s.name, s.kind, s.size, s.modified.is_some(), s.flags),
                (&l.name, l.kind, l.size, l.modified.is_some(), l.flags)
            );
        }
    }

    #[test]
    fn scan_leaves_the_table_alone_and_its_ids_are_unknown_elsewhere() {
        let (_dir, fs) = fixture();
        let browsed = fs.lookup(fs.root(), "a.txt").unwrap().unwrap();
        let before = table_len(&fs);
        let (seen, _) = scan_all(&fs, usize::MAX);
        assert_eq!(table_len(&fs), before);
        // A second scan counts from the start again.
        let (again, _) = scan_all(&fs, usize::MAX);
        assert_eq!(
            seen.iter().map(|e| e.node).min(),
            again.iter().map(|e| e.node).min()
        );

        let sub1 = seen.iter().find(|e| e.name == "sub1").unwrap().node;
        let file = seen.iter().find(|e| e.name == "a.txt").unwrap().node;
        for id in [sub1, file, FIRST_SCAN_ID, NodeId(u64::MAX)] {
            assert_eq!(fs.path(id), None);
            assert!(matches!(fs.metadata(id), Err(Error::NotFound { .. })));
            assert!(matches!(
                expect_err(fs.read_dir(id)),
                Error::NotFound { .. }
            ));
            assert!(matches!(
                fs.open(id, Stream::Data),
                Err(Error::NotFound { .. })
            ));
            assert!(matches!(
                fs.lookup(id, "inner.txt"),
                Err(Error::NotFound { .. })
            ));
        }
        // Browsing is unchanged by the scan.
        assert_eq!(fs.lookup(fs.root(), "a.txt").unwrap(), Some(browsed));
        assert_eq!(table_len(&fs), before);
    }

    /// The way the app resolves a hit on a host source: the index's crumbs
    /// give names, `lookup` from the root gives real ids.
    #[test]
    fn index_hits_resolve_through_crumbs_and_lookup() {
        use crate::search::build;
        use std::sync::atomic::AtomicBool;

        let (dir, fs) = fixture();
        let index = build(&fs, &AtomicBool::new(false), &mut |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(index.len(), 7);
        assert_eq!(table_len(&fs), 1);
        for i in 1..index.len() {
            let crumbs = index.crumbs(i);
            assert_eq!(crumbs.first().map(|c| c.0), Some(fs.root()));
            let mut node = fs.root();
            let mut path = dir.0.clone();
            for (_, name) in &crumbs[1..] {
                node = fs.lookup(node, name).unwrap().unwrap();
                path.push(name);
            }
            assert_eq!(fs.path(node), Some(path), "{}", index.name(i));
            assert_eq!(
                fs.metadata(node).unwrap().kind == NodeKind::Directory,
                index.is_folder(i)
            );
        }
    }

    #[test]
    fn scan_stops_when_the_visitor_breaks() {
        let (_dir, fs) = fixture();
        let (seen, summary) = scan_all(&fs, 2);
        assert_eq!(seen.len(), 2);
        assert_eq!(
            summary,
            ScanSummary {
                entries: 2,
                skipped: 0
            }
        );
    }

    #[test]
    fn scan_of_a_vanished_root_skips_it() {
        let (dir, fs) = fixture();
        std::fs::remove_dir_all(&dir.0).unwrap();
        let (seen, summary) = scan_all(&fs, usize::MAX);
        assert!(seen.is_empty());
        assert_eq!(
            summary,
            ScanSummary {
                entries: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn scan_of_an_empty_folder() {
        let dir = TempDir::new();
        let fs = NativeFs::new(&dir.0, "Empty").unwrap();
        let (seen, summary) = scan_all(&fs, usize::MAX);
        assert!(seen.is_empty());
        assert_eq!(summary, ScanSummary::default());
    }

    /// Links back up the tree are reported once and never entered.
    fn assert_loop_not_followed(fs: &NativeFs, link_name: &str) {
        let (seen, summary) = scan_all(fs, 10_000);
        assert_eq!(summary.skipped, 0);
        assert_eq!(seen.len(), 7, "{seen:?}");
        let link = seen.iter().find(|e| e.name == link_name).unwrap();
        assert_eq!(link.kind, NodeKind::Symlink);
        assert!(link.flags.contains(EntryFlags::LINKS_TO_DIRECTORY));
        assert!(seen.iter().all(|e| e.parent != link.node));
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_follow_a_symlink_loop() {
        let (dir, fs) = fixture();
        std::os::unix::fs::symlink(&dir.0, dir.0.join("sub1").join("up")).unwrap();
        assert_loop_not_followed(&fs, "up");
    }

    /// A directory symlink needs a privilege or developer mode on Windows;
    /// the test does nothing without it.
    #[cfg(windows)]
    #[test]
    fn scan_does_not_follow_a_symlink_loop() {
        let (dir, fs) = fixture();
        if std::os::windows::fs::symlink_dir(&dir.0, dir.0.join("sub1").join("up")).is_err() {
            return;
        }
        assert_loop_not_followed(&fs, "up");
    }

    /// A junction back to the top. `mklink /J` needs no privilege; the test
    /// does nothing where `cmd` cannot make one.
    #[cfg(windows)]
    #[test]
    fn scan_does_not_follow_a_junction_loop() {
        let (dir, fs) = fixture();
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(dir.0.join("sub1").join("up"))
            .arg(&dir.0)
            .output()
            .is_ok_and(|out| out.status.success());
        if !made {
            return;
        }
        assert_loop_not_followed(&fs, "up");
    }

    /// Linux allows any bytes but `/` and NUL in a name.
    #[cfg(target_os = "linux")]
    #[test]
    fn scan_enters_folders_whose_names_are_not_unicode() {
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new();
        let folder = dir.0.join(OsStr::from_bytes(b"dir\xfe"));
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join(OsStr::from_bytes(b"bad\xff")), b"xy").unwrap();
        let fs = NativeFs::new(&dir.0, "Bytes").unwrap();
        let (seen, summary) = scan_all(&fs, usize::MAX);
        assert_eq!(
            summary,
            ScanSummary {
                entries: 2,
                skipped: 0
            }
        );
        let folder = seen.iter().find(|e| e.parent == fs.root()).unwrap();
        assert_eq!(folder.name, "dir\u{fffd}");
        let file = seen.iter().find(|e| e.parent == folder.node).unwrap();
        assert_eq!(file.name, "bad\u{fffd}");
        assert_eq!(file.size, Some(2));
    }

    #[test]
    fn thread_bounds() {
        fn send<T: Send>() {}
        fn sync<T: Sync>() {}
        send::<NativeFs>();
        sync::<NativeFs>();
        send::<Box<dyn DirStream>>();
    }
}
