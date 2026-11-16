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

//! A filesystem that joins each `._name` AppleDouble sidecar to the file it
//! belongs to, for a volume that keeps no resource forks of its own (FAT,
//! exFAT, a server share). Source: `r10-everyday.md` section 5.2, first row.
//!
//! The join is lazy and per file. A folder listing is passed through as it
//! is, and only remembers which folder and name each entry has. When
//! `metadata` or `open` is asked about one file, the sidecar beside it is
//! looked up and read (its header only, at most
//! [`MAX_AD_HEADER`](super::appledouble::MAX_AD_HEADER) bytes). The list
//! never costs a sidecar lookup per row.
//!
//! A joined file reports the sidecar's Finder information, type and creator,
//! label and flags as its own, and its resource fork as
//! [`Stream::Resource`]. The sidecar itself stays a normal housekeeping file
//! in the listing. A sidecar that is not valid is ignored.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::Result;
use crate::fs::hfsplus::catalog::entry_flags;
use crate::fs::{
    AllocationMap, DirBatch, DirProgress, DirStream, Extent, FileSystem, FinderCodes, Metadata,
    NodeId, NodeKind, ScanEntry, ScanSummary, SnapshotInfo, Stream, VolumeIdentity, VolumeInfo,
    XattrInfo,
};
use crate::io::block::BlockSource;

use super::appledouble::{Sidecar, open_resource, read_sidecar};

/// Most entries whose folder and name are remembered. At the limit the
/// memory is dropped: a file loses its join until its folder is listed again.
const MAX_PLACES: usize = 262_144;
/// Most sidecar answers kept.
const MAX_FOUND: usize = 1024;

/// A sidecar found beside a file.
#[derive(Clone)]
struct Found {
    node: NodeId,
    sidecar: Sidecar,
}

#[derive(Default)]
struct Shared {
    /// File or folder node to (its folder, its name).
    places: Mutex<HashMap<NodeId, (NodeId, String)>>,
    /// Node to what its sidecar said (`None`: it has none).
    found: Mutex<HashMap<NodeId, Option<Found>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn remember(&self, dir: NodeId, node: NodeId, name: &str) {
        // A sidecar is not joined to anything, and `._._x` would be absurd.
        if name.starts_with("._") {
            return;
        }
        let mut places = locked(&self.places);
        if places.len() >= MAX_PLACES {
            places.clear();
        }
        places.insert(node, (dir, name.to_owned()));
    }
}

/// See the module documentation.
pub struct SidecarFs {
    inner: Arc<dyn FileSystem>,
    shared: Arc<Shared>,
}

impl SidecarFs {
    /// `fs` with its sidecars joined, or `fs` itself when the volume has
    /// forks of its own.
    pub fn wrap(fs: Arc<dyn FileSystem>) -> Arc<dyn FileSystem> {
        if fs.info().resource_forks {
            return fs;
        }
        Arc::new(Self {
            inner: fs,
            shared: Arc::new(Shared::default()),
        })
    }

    /// The sidecar beside `node`, looked up once and remembered.
    fn sidecar_of(&self, node: NodeId) -> Option<Found> {
        if let Some(known) = locked(&self.shared.found).get(&node) {
            return known.clone();
        }
        let place = locked(&self.shared.places).get(&node).cloned();
        let found = place.and_then(|(dir, name)| {
            read_sidecar(self.inner.as_ref(), dir, &name)
                .ok()
                .flatten()
                .map(|(node, sidecar)| Found { node, sidecar })
        });
        let mut cache = locked(&self.shared.found);
        if cache.len() >= MAX_FOUND {
            cache.clear();
        }
        cache.insert(node, found.clone());
        found
    }
}

/// Passes a listing through and notes where each entry is.
struct Noting {
    inner: Box<dyn DirStream>,
    dir: NodeId,
    shared: Arc<Shared>,
}

impl DirStream for Noting {
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
        let start = out.len();
        let progress = self.inner.next_batch(out, max);
        // Entries appended before an error are valid, so they are noted too.
        for index in start..out.len() {
            if let Some(entry) = out.get(index)
                && matches!(entry.kind, NodeKind::File | NodeKind::Directory)
            {
                self.shared.remember(self.dir, entry.node, entry.name);
            }
        }
        progress
    }
}

/// The type and creator in 32 bytes of Finder information, `None` when both are zero.
fn codes_of(info: &[u8; 32]) -> Option<FinderCodes> {
    let file_type: [u8; 4] = info.first_chunk::<4>().copied()?;
    let creator: [u8; 4] = info.get(4..)?.first_chunk::<4>().copied()?;
    (file_type != [0; 4] || creator != [0; 4]).then_some(FinderCodes { file_type, creator })
}

impl FileSystem for SidecarFs {
    fn info(&self) -> VolumeInfo {
        let mut info = self.inner.info();
        // The forks are in the sidecars: the panel may show them.
        info.resource_forks = true;
        info
    }

    fn root(&self) -> NodeId {
        self.inner.root()
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata> {
        let mut meta = self.inner.metadata(node)?;
        if !matches!(meta.kind, NodeKind::File | NodeKind::Directory) {
            return Ok(meta);
        }
        let Some(found) = self.sidecar_of(node) else {
            return Ok(meta);
        };
        if let Some(info) = found.sidecar.finder_info
            && info != [0; 32]
        {
            meta.finder_info = Some(info);
            if meta.kind == NodeKind::File {
                meta.finder_codes = codes_of(&info);
            }
            let flags = info
                .get(8..10)
                .and_then(|pair| <[u8; 2]>::try_from(pair).ok())
                .map_or(0, u16::from_be_bytes);
            meta.flags = meta.flags.with(entry_flags(false, flags));
        }
        if meta.kind == NodeKind::File
            && let Some((_, len)) = found.sidecar.resource
        {
            meta.resource_size = Some(len);
        }
        Ok(meta)
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
        let found = self.inner.lookup(dir, name)?;
        if let Some(node) = found {
            self.shared.remember(dir, node, name);
        }
        Ok(found)
    }

    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
        Ok(Box::new(Noting {
            inner: self.inner.read_dir(dir)?,
            dir,
            shared: Arc::clone(&self.shared),
        }))
    }

    fn extents(&self, node: NodeId, stream: Stream) -> Result<Vec<Extent>> {
        match stream {
            Stream::Data => self.inner.extents(node, stream),
            // Inside another file; nothing on the volume to mark for the fork itself.
            Stream::Resource => Ok(Vec::new()),
        }
    }

    fn allocation_map(&self, buckets: usize) -> Result<AllocationMap> {
        self.inner.allocation_map(buckets)
    }

    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        if stream == Stream::Resource
            && let Some(found) = self.sidecar_of(node)
            && let Some(at) = found.sidecar.resource
            && at.1 > 0
        {
            return open_resource(self.inner.as_ref(), found.node, at);
        }
        self.inner.open(node, stream)
    }

    fn xattrs(&self, node: NodeId) -> Result<Vec<XattrInfo>> {
        self.inner.xattrs(node)
    }

    fn read_xattr(&self, node: NodeId, name: &str, max: usize) -> Result<Option<Vec<u8>>> {
        self.inner.read_xattr(node, name, max)
    }

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        self.inner.snapshots()
    }

    fn open_snapshot(&self, id: u64) -> Result<Arc<dyn FileSystem>> {
        self.inner.open_snapshot(id)
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(ScanEntry<'_>) -> std::ops::ControlFlow<()>,
    ) -> Result<ScanSummary> {
        self.inner.scan(visit)
    }

    fn identity(&self) -> Option<VolumeIdentity> {
        self.inner.identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::appledouble::header;
    use crate::fs::{DirEntry, EntryFlags};
    use crate::io::block::MemSource;

    /// A volume with no forks: a root with `Notes`, its sidecar `._Notes`,
    /// `Plain`, and `._Bad` beside `Bad` (a damaged sidecar).
    struct Flat {
        files: Vec<(u64, &'static str, Vec<u8>)>,
        forks: bool,
    }

    impl FileSystem for Flat {
        fn info(&self) -> VolumeInfo {
            let mut info = VolumeInfo::new("flat");
            info.resource_forks = self.forks;
            info
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            let (_, _, bytes) = self
                .files
                .iter()
                .find(|(id, _, _)| *id == node.0)
                .ok_or(crate::error::Error::NotFound { node: node.0 })?;
            Ok(Metadata {
                node,
                kind: NodeKind::File,
                size: Some(bytes.len() as u64),
                resource_size: None,
                created: None,
                modified: None,
                accessed: None,
                flags: EntryFlags::EMPTY,
                link_target: None,
                finder_codes: None,
                owner: None,
                attributes: None,
                finder_info: None,
            })
        }
        fn lookup(&self, _dir: NodeId, name: &str) -> Result<Option<NodeId>> {
            Ok(self
                .files
                .iter()
                .find(|(_, n, _)| *n == name)
                .map(|(id, _, _)| NodeId(*id)))
        }
        fn read_dir(&self, _dir: NodeId) -> Result<Box<dyn DirStream>> {
            struct One(Option<Vec<(u64, &'static str, usize)>>);
            impl DirStream for One {
                fn next_batch(&mut self, out: &mut DirBatch, _max: usize) -> Result<DirProgress> {
                    for (id, name, len) in self.0.take().unwrap_or_default() {
                        out.push(DirEntry {
                            node: NodeId(id),
                            name,
                            kind: NodeKind::File,
                            size: Some(len as u64),
                            resource_size: None,
                            modified: None,
                            flags: EntryFlags::EMPTY,
                        });
                    }
                    Ok(DirProgress::Done)
                }
            }
            Ok(Box::new(One(Some(
                self.files
                    .iter()
                    .map(|(id, n, b)| (*id, *n, b.len()))
                    .collect(),
            ))))
        }
        fn open(&self, node: NodeId, _stream: Stream) -> Result<Arc<dyn BlockSource>> {
            let (_, _, bytes) = self
                .files
                .iter()
                .find(|(id, _, _)| *id == node.0)
                .ok_or(crate::error::Error::NotFound { node: node.0 })?;
            Ok(Arc::new(MemSource::new(bytes.clone())))
        }
    }

    fn volume(forks: bool) -> Flat {
        // TEXT/ttxt, flags with label 6 (Red): kColor is bits 1 to 3.
        let mut finder = [0u8; 32];
        finder[..8].copy_from_slice(b"TEXTttxt");
        finder[8..10].copy_from_slice(&(6u16 << 1).to_be_bytes());
        let mut sidecar = header(Some(&finder), 4).unwrap();
        sidecar.extend_from_slice(b"RSRC");
        let mut bad = sidecar.clone();
        // The fork entry points far past the end.
        bad[38 + 4..38 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        Flat {
            files: vec![
                (2, "Notes", b"hello".to_vec()),
                (3, "._Notes", sidecar),
                (4, "Plain", b"x".to_vec()),
                (5, "Bad", b"y".to_vec()),
                (6, "._Bad", bad),
            ],
            forks,
        }
    }

    fn listed(fs: &Arc<dyn FileSystem>) {
        let mut stream = fs.read_dir(fs.root()).unwrap();
        let mut batch = DirBatch::new();
        stream.next_batch(&mut batch, 100).unwrap();
    }

    #[test]
    fn a_sidecar_gives_its_file_a_fork_type_and_label() {
        let fs = SidecarFs::wrap(Arc::new(volume(false)));
        assert!(fs.info().resource_forks);
        listed(&fs);
        let meta = fs.metadata(NodeId(2)).unwrap();
        assert_eq!(meta.resource_size, Some(4));
        assert_eq!(meta.finder_codes.unwrap().file_type, *b"TEXT");
        assert_eq!(meta.flags.label(), 6);
        let fork = fs.open(NodeId(2), Stream::Resource).unwrap();
        let mut bytes = [0u8; 4];
        fork.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"RSRC");
        // The data fork is untouched.
        assert_eq!(fs.open(NodeId(2), Stream::Data).unwrap().len(), 5);
    }

    #[test]
    fn files_without_a_usable_sidecar_are_left_alone() {
        let fs = SidecarFs::wrap(Arc::new(volume(false)));
        listed(&fs);
        for node in [NodeId(4), NodeId(5)] {
            let meta = fs.metadata(node).unwrap();
            assert_eq!(meta.resource_size, None);
            assert_eq!(meta.finder_info, None);
        }
        // Before the folder was listed nothing is joined either.
        let cold = SidecarFs::wrap(Arc::new(volume(false)));
        assert_eq!(cold.metadata(NodeId(2)).unwrap().resource_size, None);
    }

    #[test]
    fn a_volume_with_forks_is_not_wrapped() {
        let fs: Arc<dyn FileSystem> = Arc::new(volume(true));
        let wrapped = SidecarFs::wrap(Arc::clone(&fs));
        assert!(Arc::ptr_eq(&fs, &wrapped));
    }
}
