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

//! "Show original": where a symbolic link or a classic alias points, on the
//! same volume. Source: `r10-everyday.md` section 5.4, second item.
//!
//! A symbolic link is resolved from the folder that holds it (or from the
//! root when absolute) with `lookup`, one name at a time, never leaving the
//! volume. A classic alias is read from its `alis` resource: its path is
//! walked from the root, else its file number is looked up directly. A link
//! that leads to another link is followed, at most [`MAX_LINK_STEPS`] times.

use crate::error::{Error, Result};
use crate::fs::{EntryFlags, FileSystem, Metadata, NodeId, NodeKind, Stream};

use super::alias::{MAX_ALIAS, parse_alias};
use super::resource_map::ResourceMap;

/// Most links followed, and most path segments walked.
pub const MAX_LINK_STEPS: usize = 64;

/// A found original: the folder holding it as crumbs below the root, and its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    /// Folders from the root (not including it) to the one that holds the original.
    pub folder: Vec<(NodeId, String)>,
    /// The original's name.
    pub name: String,
    /// The original's node.
    pub node: NodeId,
}

/// The outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The original is on this volume.
    Found(Original),
    /// It is on a disk with this name.
    OtherVolume(String),
}

/// True when `meta` describes something this module can follow.
pub fn is_link(meta: &Metadata) -> bool {
    meta.kind == NodeKind::Symlink
        || (meta.kind == NodeKind::File && meta.flags.contains(EntryFlags::ALIAS))
}

/// One step: where `node` (called `name`, held by the folder `dir`) points.
enum Next {
    Target(Vec<(NodeId, String)>, String, NodeId),
    Other(String),
}

fn words(text: &'static str) -> Error {
    Error::unsupported(text)
}

/// Follows the link or alias `node` until something that is not one. `from` is the folder holding
/// it, as crumbs below the root.
pub fn resolve(
    fs: &dyn FileSystem,
    from: &[(NodeId, String)],
    name: &str,
    node: NodeId,
) -> Result<Resolved> {
    let (mut dir, mut name, mut node) = (from.to_vec(), name.to_owned(), node);
    for _ in 0..MAX_LINK_STEPS {
        let meta = fs.metadata(node)?;
        if !is_link(&meta) {
            // Not a link at the first step: nothing to follow.
            return Ok(Resolved::Found(Original {
                folder: dir,
                name,
                node,
            }));
        }
        match follow(fs, &dir, &meta)? {
            Next::Other(volume) => return Ok(Resolved::OtherVolume(volume)),
            Next::Target(next_dir, next_name, next_node) => {
                dir = next_dir;
                name = next_name;
                node = next_node;
            }
        }
    }
    Err(words("This link leads back to itself."))
}

fn follow(fs: &dyn FileSystem, dir: &[(NodeId, String)], meta: &Metadata) -> Result<Next> {
    if meta.kind == NodeKind::Symlink {
        let target = meta
            .link_target
            .as_deref()
            .ok_or_else(|| words("This link does not say where it points."))?;
        let start: Vec<(NodeId, String)> = if target.starts_with('/') {
            Vec::new()
        } else {
            dir.to_vec()
        };
        let segments: Vec<&str> = target.split('/').collect();
        return walk(fs, start, &segments);
    }
    follow_alias(fs, meta.node)
}

/// Walks `segments` from the folder `start`; the last one is the original.
fn walk(fs: &dyn FileSystem, start: Vec<(NodeId, String)>, segments: &[&str]) -> Result<Next> {
    let mut path = start;
    let mut last: Option<(String, NodeId)> = None;
    for segment in segments.iter().take(MAX_LINK_STEPS) {
        if segment.is_empty() || *segment == "." {
            continue;
        }
        // The previous segment becomes a folder of the path.
        if let Some((name, node)) = last.take() {
            path.push((node, name));
        }
        if *segment == ".." {
            if path.pop().is_none() {
                return Err(words("This link points out of the disk."));
            }
            continue;
        }
        let here = path.last().map_or_else(|| fs.root(), |(node, _)| *node);
        let found = fs
            .lookup(here, segment)?
            .ok_or_else(|| words("The original could not be found on this disk."))?;
        last = Some(((*segment).to_owned(), found));
    }
    match last {
        Some((name, node)) => Ok(Next::Target(path, name, node)),
        // "/" or "..": the target is a folder, shown from its parent.
        None => {
            let (node, name) = path
                .pop()
                .ok_or_else(|| words("This link points at the top of the disk."))?;
            Ok(Next::Target(path, name, node))
        }
    }
}

fn follow_alias(fs: &dyn FileSystem, node: NodeId) -> Result<Next> {
    let fork = fs
        .open(node, Stream::Resource)
        .map_err(|_| words("This alias has no record of its original."))?;
    let map = ResourceMap::read(fork.as_ref())?;
    let record = map
        .load(fork.as_ref(), *b"alis", 0, MAX_ALIAS)?
        .ok_or_else(|| words("This alias has no record of its original."))?;
    let target = parse_alias(&record)?;
    if target.other_volume {
        return Ok(Next::Other(target.volume_name));
    }
    // By path from the root, dropping the volume's own name when the path starts with it.
    if let Some(path) = &target.path {
        let mut segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if segments.first() == Some(&"Volumes") {
            segments.remove(0);
        }
        if segments
            .first()
            .is_some_and(|first| first.eq_ignore_ascii_case(&target.volume_name))
        {
            segments.remove(0);
        }
        if !segments.is_empty()
            && let Ok(next @ Next::Target(..)) = walk(fs, Vec::new(), &segments)
        {
            return Ok(next);
        }
    }
    // By file number: the folder is known by its number only.
    let meta = fs
        .metadata(target.node)
        .map_err(|_| words("The original could not be found on this disk."))?;
    if target.name.is_empty() {
        return Err(words("The original could not be found on this disk."));
    }
    Ok(Next::Target(
        vec![(target.parent, "Original's folder".to_owned())],
        target.name,
        meta.node,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{DirStream, VolumeInfo};
    use crate::io::block::{BlockSource, MemSource};
    use crate::mac::alias::build_record;
    use crate::mac::resource_map::test_fork::build;
    use std::sync::Arc;

    struct N {
        parent: u64,
        name: &'static str,
        kind: NodeKind,
        link: Option<&'static str>,
        alias: Option<Vec<u8>>,
    }

    struct Tree(Vec<(u64, N)>);

    fn dir(parent: u64, name: &'static str) -> N {
        N {
            parent,
            name,
            kind: NodeKind::Directory,
            link: None,
            alias: None,
        }
    }
    fn file(parent: u64, name: &'static str) -> N {
        N {
            parent,
            name,
            kind: NodeKind::File,
            link: None,
            alias: None,
        }
    }
    fn symlink(parent: u64, name: &'static str, to: &'static str) -> N {
        N {
            parent,
            name,
            kind: NodeKind::Symlink,
            link: Some(to),
            alias: None,
        }
    }

    impl FileSystem for Tree {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("Disk")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            let (_, n) = self
                .0
                .iter()
                .find(|(id, _)| *id == node.0)
                .ok_or(Error::NotFound { node: node.0 })?;
            Ok(Metadata {
                node,
                kind: n.kind,
                size: None,
                resource_size: None,
                created: None,
                modified: None,
                accessed: None,
                flags: if n.alias.is_some() {
                    EntryFlags::ALIAS
                } else {
                    EntryFlags::EMPTY
                },
                link_target: n.link.map(str::to_owned),
                finder_codes: None,
                owner: None,
                attributes: None,
                finder_info: None,
            })
        }
        fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
            Ok(self
                .0
                .iter()
                .find(|(_, n)| n.parent == dir.0 && n.name == name)
                .map(|(id, _)| NodeId(*id)))
        }
        fn read_dir(&self, _dir: NodeId) -> Result<Box<dyn DirStream>> {
            Err(Error::unsupported("not needed"))
        }
        fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
            let alias = self
                .0
                .iter()
                .find(|(id, _)| *id == node.0)
                .and_then(|(_, n)| n.alias.clone())
                .filter(|_| stream == Stream::Resource)
                .ok_or(Error::NoSuchStream { node: node.0 })?;
            Ok(Arc::new(MemSource::new(build(&[(
                *b"alis",
                vec![(0, None, &alias)],
            )]))))
        }
    }

    fn base() -> Vec<(u64, N)> {
        vec![
            (2, dir(1, "Docs")),
            (3, file(2, "Report")),
            (4, symlink(1, "latest", "Docs/Report")),
            (5, symlink(2, "up", "../Docs/./Report")),
            (6, symlink(1, "abs", "/Docs/Report")),
            (7, symlink(1, "loop", "loop")),
            (8, symlink(1, "outside", "../../x")),
            (9, symlink(1, "chain", "latest")),
        ]
    }

    #[test]
    fn symbolic_links_relative_absolute_and_chained() {
        let fs = Tree(base());
        let root: Vec<(NodeId, String)> = Vec::new();
        let docs = vec![(NodeId(2), "Docs".to_owned())];
        let found = |from: &[(NodeId, String)], name: &str, node: u64| {
            resolve(&fs, from, name, NodeId(node)).unwrap()
        };
        let expect = Resolved::Found(Original {
            folder: docs.clone(),
            name: "Report".into(),
            node: NodeId(3),
        });
        assert_eq!(found(&root, "latest", 4), expect);
        assert_eq!(found(&docs, "up", 5), expect);
        assert_eq!(found(&docs, "abs", 6), expect);
        // A link to a link.
        assert_eq!(found(&root, "chain", 9), expect);
    }

    #[test]
    fn a_link_to_itself_and_one_leading_out_are_errors_in_words() {
        let fs = Tree(base());
        let loop_error = resolve(&fs, &[], "loop", NodeId(7)).unwrap_err();
        assert_eq!(
            loop_error.to_string(),
            "not supported: This link leads back to itself."
        );
        assert!(resolve(&fs, &[], "outside", NodeId(8)).is_err());
        // A missing target.
        let mut nodes = base();
        nodes.push((10, symlink(1, "dangling", "Nowhere")));
        assert!(resolve(&Tree(nodes), &[], "dangling", NodeId(10)).is_err());
    }

    #[test]
    fn an_alias_by_path_by_number_and_on_another_volume() {
        let mut nodes = base();
        let by_path = build_record("Report", "Disk", 2, 3, &[(2, b"Disk:Docs:Report")]);
        let by_number = build_record("Report", "Disk", 2, 3, &[]);
        let mut elsewhere = build_record("Report", "Archive 2009", 2, 3, &[]);
        elsewhere[130..132].copy_from_slice(&(-1i16).to_be_bytes());
        for (id, name, record) in [
            (20, "a", by_path),
            (21, "b", by_number),
            (22, "c", elsewhere),
        ] {
            let mut node = file(1, name);
            node.alias = Some(record);
            nodes.push((id, node));
        }
        let fs = Tree(nodes);
        let report = |folder: Vec<(NodeId, String)>| {
            Resolved::Found(Original {
                folder,
                name: "Report".into(),
                node: NodeId(3),
            })
        };
        assert_eq!(
            resolve(&fs, &[], "a", NodeId(20)).unwrap(),
            report(vec![(NodeId(2), "Docs".into())])
        );
        // By number the folder is known by its number only.
        assert_eq!(
            resolve(&fs, &[], "b", NodeId(21)).unwrap(),
            report(vec![(NodeId(2), "Original's folder".into())])
        );
        assert_eq!(
            resolve(&fs, &[], "c", NodeId(22)).unwrap(),
            Resolved::OtherVolume("Archive 2009".into())
        );
    }
}
