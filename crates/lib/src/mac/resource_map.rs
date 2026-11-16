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

//! Reader for the resource map of a classic Mac OS resource fork: the list
//! of (type, id) resources, their names and where their data lies. Source:
//! `r10-everyday.md` section 5.3, checked against *Inside Macintosh: More
//! Macintosh Toolbox* and the `python-rsrcfork` reader; every integer is
//! big-endian.
//!
//! ```text
//! fork + 0      data offset u32, map offset u32, data length u32, map length u32
//! map + 22      file attributes u16
//! map + 24      type list offset u16 (from the start of the map)
//! map + 26      name list offset u16 (from the start of the map)
//! type list + 0 number of types - 1 (0xFFFF: none); then 8 bytes each:
//!               type[4], resources - 1 u16, reference list offset u16
//!               (from the start of the type list)
//! reference     id i16, name offset u16 (0xFFFF: none), attributes u8,
//!               data offset u24 (from the start of the resource data), 4 ignored
//! data          length u32, then the bytes
//! name          length u8, then Mac OS Roman
//! ```

use crate::error::{CorruptKind, Error, Result};
use crate::fs::hfs::encoding::Encoding;
use crate::io::block::BlockSource;

use super::util::{be16, be32, corrupt, len64, to_usize, truncated};

const WHAT: &str = "resource map";

/// Most bytes of map read.
pub const MAX_MAP_BYTES: usize = 1 << 20;
/// Most resource types.
pub const MAX_TYPES: usize = 2048;
/// Most resources of one type.
pub const MAX_PER_TYPE: usize = 16_384;
/// Most resources in all.
pub const MAX_RESOURCES: usize = 65_536;

const HEADER: usize = 16;
/// The map's fixed part: header copy, handle, file reference, attributes, two offsets.
const MAP_FIXED: usize = 28;

#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: [u8; 4],
    id: i16,
    /// From the start of the resource data.
    data_offset: u32,
    name_offset: Option<u16>,
}

/// The index of one fork's resources. It holds the map, not the data.
#[derive(Debug, Clone)]
pub struct ResourceMap {
    /// Sorted by (type, id).
    entries: Vec<Entry>,
    data_start: u64,
    data_len: u64,
    /// The name list.
    names: Vec<u8>,
}

fn invalid(offset: u64, field: &'static str, value: u64) -> Error {
    corrupt(WHAT, offset, CorruptKind::InvalidValue { field, value })
}

impl ResourceMap {
    /// Reads the header and the map (at most [`MAX_MAP_BYTES`]), no resource data.
    pub fn read(fork: &dyn BlockSource) -> Result<Self> {
        let fork_len = fork.len();
        let mut header = [0u8; HEADER];
        if fork_len < HEADER as u64 {
            return Err(truncated(WHAT, 0, HEADER as u64, fork_len));
        }
        fork.read_at(0, &mut header)?;
        let word = |at| u64::from(be32(&header, at).unwrap_or(0));
        let (data_off, map_off, data_len, map_len) = (word(0), word(4), word(8), word(12));
        let end = |offset: u64, len: u64| offset.checked_add(len).filter(|&end| end <= fork_len);
        let (Some(data_end), Some(map_end)) = (end(data_off, data_len), end(map_off, map_len))
        else {
            return Err(truncated(WHAT, 0, data_off.max(map_off), fork_len));
        };
        if map_len < MAP_FIXED as u64 || map_len > MAX_MAP_BYTES as u64 {
            return Err(invalid(12, "map length", map_len));
        }
        if data_off < HEADER as u64 || map_off < HEADER as u64 {
            return Err(invalid(0, "data or map offset", data_off.min(map_off)));
        }
        // Data and map must not overlap.
        if data_off < map_end && map_off < data_end {
            return Err(invalid(4, "map offset", map_off));
        }
        let mut map = vec![0u8; to_usize(map_len)];
        fork.read_at(map_off, &mut map)?;

        let type_list = usize::from(be16(&map, 24).unwrap_or(0));
        let name_list = usize::from(be16(&map, 26).unwrap_or(0));
        if type_list < MAP_FIXED || type_list.saturating_add(2) > map.len() {
            return Err(invalid(map_off, "type list offset", type_list as u64));
        }
        let types = match be16(&map, type_list) {
            Some(0xFFFF) | None => 0,
            Some(count) => usize::from(count).saturating_add(1),
        };
        if types > MAX_TYPES {
            return Err(corrupt(
                WHAT,
                map_off,
                CorruptKind::OutOfRange {
                    field: "type count",
                    value: types as u64,
                    limit_name: "most types",
                    limit: MAX_TYPES as u64,
                },
            ));
        }
        let mut entries: Vec<Entry> = Vec::new();
        for index in 0..types {
            let at = type_list
                .saturating_add(2)
                .saturating_add(index.saturating_mul(8));
            let (Some(kind), Some(count), Some(refs)) = (
                map.get(at..).and_then(<[u8]>::first_chunk::<4>).copied(),
                be16(&map, at.saturating_add(4)),
                be16(&map, at.saturating_add(6)),
            ) else {
                return Err(truncated(WHAT, at as u64, 8, len64(&map)));
            };
            let count = usize::from(count).saturating_add(1);
            if count > MAX_PER_TYPE || entries.len().saturating_add(count) > MAX_RESOURCES {
                return Err(corrupt(
                    WHAT,
                    at as u64,
                    CorruptKind::LimitExceeded {
                        what: "resources",
                        limit: MAX_RESOURCES as u64,
                    },
                ));
            }
            let first = type_list.saturating_add(usize::from(refs));
            for n in 0..count {
                let at = first.saturating_add(n.saturating_mul(12));
                let (Some(id), Some(name), Some(packed)) = (
                    be16(&map, at),
                    be16(&map, at.saturating_add(2)),
                    be32(&map, at.saturating_add(4)),
                ) else {
                    return Err(truncated(WHAT, at as u64, 12, len64(&map)));
                };
                // The last three bytes of `packed` are the data offset: byte 4 is the attributes.
                let data_offset = packed & 0x00FF_FFFF;
                if u64::from(data_offset).saturating_add(4) > data_len {
                    return Err(invalid(
                        at as u64,
                        "resource data offset",
                        u64::from(data_offset),
                    ));
                }
                entries.push(Entry {
                    kind,
                    id: i16::from_be_bytes(id.to_be_bytes()),
                    data_offset,
                    name_offset: (name != 0xFFFF).then_some(name),
                });
            }
        }
        entries.sort_by_key(|entry| (entry.kind, entry.id));
        let names = map
            .get(name_list.min(map.len())..)
            .unwrap_or_default()
            .to_vec();
        Ok(Self {
            entries,
            data_start: data_off,
            data_len,
            names,
        })
    }

    /// The types in the fork and how many resources each has, in order.
    pub fn types(&self) -> impl Iterator<Item = ([u8; 4], usize)> + '_ {
        let mut out: Vec<([u8; 4], usize)> = Vec::new();
        for entry in &self.entries {
            match out.last_mut() {
                Some((kind, count)) if *kind == entry.kind => *count = count.saturating_add(1),
                _ => out.push((entry.kind, 1)),
            }
        }
        out.into_iter()
    }

    /// The ids of the resources of `kind`, in order.
    pub fn ids(&self, kind: [u8; 4]) -> impl Iterator<Item = i16> + '_ {
        self.entries
            .iter()
            .filter(move |entry| entry.kind == kind)
            .map(|entry| entry.id)
    }

    fn find(&self, kind: [u8; 4], id: i16) -> Option<&Entry> {
        let at = self
            .entries
            .partition_point(|entry| (entry.kind, entry.id) < (kind, id));
        self.entries
            .get(at)
            .filter(|entry| entry.kind == kind && entry.id == id)
    }

    /// The name of a resource, when it has one (Mac OS Roman).
    pub fn name(&self, kind: [u8; 4], id: i16) -> Option<String> {
        let offset = usize::from(self.find(kind, id)?.name_offset?);
        let len = usize::from(*self.names.get(offset)?);
        let bytes = self.names.get(offset.checked_add(1)?..)?.get(..len)?;
        let mut text = String::new();
        Encoding::Roman.decode_into(bytes, &mut text);
        Some(text)
    }

    /// The data of a resource. `Ok(None)` when there is no such resource; `Err` when it is longer
    /// than `max` or its length runs past the data.
    pub fn load(
        &self,
        fork: &dyn BlockSource,
        kind: [u8; 4],
        id: i16,
        max: usize,
    ) -> Result<Option<Vec<u8>>> {
        let Some(entry) = self.find(kind, id) else {
            return Ok(None);
        };
        let at = self.data_start.saturating_add(u64::from(entry.data_offset));
        let mut len = [0u8; 4];
        fork.read_at(at, &mut len)?;
        let len = u64::from(u32::from_be_bytes(len));
        let room = self
            .data_len
            .saturating_sub(u64::from(entry.data_offset))
            .saturating_sub(4);
        if len > room {
            return Err(truncated(WHAT, at, len, room));
        }
        if len > max as u64 {
            return Err(corrupt(
                WHAT,
                at,
                CorruptKind::LimitExceeded {
                    what: "resource size",
                    limit: max as u64,
                },
            ));
        }
        let mut bytes = vec![0u8; to_usize(len)];
        fork.read_at(at.saturating_add(4), &mut bytes)?;
        Ok(Some(bytes))
    }
}

/// Builds resource forks for tests and fuzz seeds.
#[cfg(test)]
pub(crate) mod test_fork {
    /// One resource: id, optional name, data.
    pub type Res<'a> = (i16, Option<&'a str>, &'a [u8]);

    /// A well-formed resource fork holding `types`.
    pub fn build(types: &[([u8; 4], Vec<Res<'_>>)]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut names = Vec::new();
        // Per type: (kind, [(id, name offset, data offset)]).
        let mut layout = Vec::new();
        for (kind, resources) in types {
            let mut refs = Vec::new();
            for (id, name, bytes) in resources {
                let data_offset = data.len() as u32;
                data.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                data.extend_from_slice(bytes);
                let name_offset = name.map(|name| {
                    let at = names.len() as u16;
                    names.push(name.len() as u8);
                    names.extend_from_slice(name.as_bytes());
                    at
                });
                refs.push((*id, name_offset, data_offset));
            }
            layout.push((*kind, refs));
        }
        let type_list_len = 2 + 8 * layout.len();
        let refs_total: usize = layout.iter().map(|(_, refs)| refs.len() * 12).sum();
        let name_list_off = 28 + type_list_len + refs_total;
        let mut map = vec![0u8; 24];
        map.extend_from_slice(&28u16.to_be_bytes());
        map.extend_from_slice(&(name_list_off as u16).to_be_bytes());
        map.extend_from_slice(&(layout.len() as u16).wrapping_sub(1).to_be_bytes());
        let mut ref_at = type_list_len;
        for (kind, refs) in &layout {
            map.extend_from_slice(kind);
            map.extend_from_slice(&(refs.len() as u16 - 1).to_be_bytes());
            map.extend_from_slice(&(ref_at as u16).to_be_bytes());
            ref_at += refs.len() * 12;
        }
        for (_, refs) in &layout {
            for (id, name, data_offset) in refs {
                map.extend_from_slice(&id.to_be_bytes());
                map.extend_from_slice(&name.unwrap_or(0xFFFF).to_be_bytes());
                map.push(0);
                map.extend_from_slice(&data_offset.to_be_bytes()[1..]);
                map.extend_from_slice(&[0; 4]);
            }
        }
        map.extend_from_slice(&names);
        let mut fork = Vec::new();
        fork.extend_from_slice(&256u32.to_be_bytes());
        fork.extend_from_slice(&(256 + data.len() as u32).to_be_bytes());
        fork.extend_from_slice(&(data.len() as u32).to_be_bytes());
        fork.extend_from_slice(&(map.len() as u32).to_be_bytes());
        fork.resize(256, 0);
        fork.extend_from_slice(&data);
        fork.extend_from_slice(&map);
        fork
    }
}

#[cfg(test)]
mod tests {
    use super::test_fork::build;
    use super::*;
    use crate::io::block::MemSource;

    fn open(bytes: Vec<u8>) -> MemSource {
        MemSource::new(bytes)
    }

    #[test]
    fn two_types_and_a_named_resource() {
        let fork = open(build(&[
            (
                *b"STR ",
                vec![(128, Some("Greeting"), b"hello"), (-1, None, b"")],
            ),
            (*b"alis", vec![(0, None, b"ALIAS")]),
        ]));
        let map = ResourceMap::read(&fork).unwrap();
        assert_eq!(
            map.types().collect::<Vec<_>>(),
            [(*b"STR ", 2), (*b"alis", 1)]
        );
        assert_eq!(map.ids(*b"STR ").collect::<Vec<_>>(), [-1, 128]);
        assert_eq!(map.name(*b"STR ", 128).as_deref(), Some("Greeting"));
        assert_eq!(map.name(*b"STR ", -1), None);
        assert_eq!(
            map.load(&fork, *b"STR ", 128, 100).unwrap().unwrap(),
            b"hello"
        );
        assert_eq!(map.load(&fork, *b"STR ", -1, 100).unwrap().unwrap(), b"");
        assert_eq!(
            map.load(&fork, *b"alis", 0, 100).unwrap().unwrap(),
            b"ALIAS"
        );
        assert!(map.load(&fork, *b"nope", 0, 100).unwrap().is_none());
        // Longer than the caller allows.
        assert!(map.load(&fork, *b"STR ", 128, 3).is_err());
    }

    #[test]
    fn an_empty_map_has_no_types() {
        // The type list count is stored as 0xFFFF.
        let fork = build(&[]);
        let map = ResourceMap::read(&open(fork)).unwrap();
        assert_eq!(map.types().count(), 0);
    }

    #[test]
    fn hostile_forks_are_errors() {
        let good = build(&[(*b"STR ", vec![(1, None, b"abc")])]);
        let read = |bytes: Vec<u8>| ResourceMap::read(&open(bytes));
        assert!(read(good.clone()).is_ok());
        // Too short, and nothing at all.
        assert!(read(good[..10].to_vec()).is_err());
        assert!(read(Vec::new()).is_err());
        // The map runs past the fork.
        let mut long_map = good.clone();
        long_map[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(read(long_map).is_err());
        // The map overlaps the data.
        let mut overlap = good.clone();
        overlap[4..8].copy_from_slice(&256u32.to_be_bytes());
        assert!(read(overlap).is_err());
        // A reference list that points outside the map.
        let map_off = u32::from_be_bytes(good[4..8].try_into().unwrap()) as usize;
        let mut outside = good.clone();
        outside[map_off + 28 + 2 + 6..map_off + 28 + 2 + 8]
            .copy_from_slice(&0xFFF0u16.to_be_bytes());
        assert!(read(outside).is_err());
        // A resource whose data offset is past the data.
        let mut far = good.clone();
        let ref_at = map_off + 28 + 2 + 8;
        far[ref_at + 5..ref_at + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(read(far).is_err());
        // A resource whose length runs past the data fails at load, not at read.
        let mut liar = good.clone();
        liar[256..260].copy_from_slice(&1000u32.to_be_bytes());
        let source = open(liar);
        let map = ResourceMap::read(&source).unwrap();
        assert!(map.load(&source, *b"STR ", 1, 10_000).is_err());
        // 2,049 types.
        let mut many = good;
        many[map_off + 28..map_off + 30].copy_from_slice(&2048u16.to_be_bytes());
        assert!(read(many).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let base = build(&[(*b"icns", vec![(-16455, Some("Icon"), b"data")])]);
        for _ in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..(next() % 6) {
                let at = (next() % bytes.len() as u64) as usize;
                bytes[at] = next() as u8;
            }
            let source = open(bytes);
            if let Ok(map) = ResourceMap::read(&source) {
                for (kind, _) in map.types().collect::<Vec<_>>() {
                    for id in map.ids(kind).collect::<Vec<_>>() {
                        let _ = map.load(&source, kind, id, 1 << 16);
                        let _ = map.name(kind, id);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::test_fork::build;

    /// Writes seeds for the `resource_map` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/resource_map`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("resource_map");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        let fork = build(&[
            (
                *b"STR ",
                vec![(128, Some("Greeting"), b"hello"), (-1, None, b"")],
            ),
            (*b"alis", vec![(0, None, b"ALIAS")]),
        ]);
        assert!(super::ResourceMap::read(&crate::io::block::MemSource::new(fork.clone())).is_ok());
        std::fs::write(dir.join("two_types"), fork).expect("write seed");
        std::fs::write(dir.join("empty"), build(&[])).expect("write seed");
    }
}
