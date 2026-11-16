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

//! The extended attributes macOS puts on files, read into plain values:
//! where a download came from, when and by what, user tags, and the Finder
//! comment. Source: `r10-everyday.md` section 5.3.

use crate::error::Result;
use crate::fs::{FileSystem, NodeId};
use crate::image::plist::Value;

use super::bplist::parse_any;

/// Most bytes of one attribute value that are read.
pub const MAX_XATTR: usize = 64 * 1024;
/// Most tags and most origins taken from one file.
pub const MAX_ITEMS: usize = 64;

/// The download quarantine record.
pub const QUARANTINE: &str = "com.apple.quarantine";
/// The list of addresses a file came from.
pub const WHERE_FROMS: &str = "com.apple.metadata:kMDItemWhereFroms";
/// The user's tags.
pub const USER_TAGS: &str = "com.apple.metadata:_kMDItemUserTags";
/// The Finder comment.
pub const FINDER_COMMENT: &str = "com.apple.metadata:kMDItemFinderComment";

/// `com.apple.quarantine`: `flags;hex seconds since 1970;agent;uuid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    /// The flag word, as the hex field says.
    pub flags: u16,
    /// When the file was downloaded, seconds since 1970 (UTC). `None` when
    /// the field is not hex.
    pub unix_seconds: Option<u64>,
    /// The application that downloaded it ("Safari").
    pub agent: String,
    /// The event's UUID, when the record has a fourth field.
    pub uuid: Option<String>,
}

/// Parses the text of a quarantine attribute. `None` when it has fewer than
/// three fields or the flags are not hex.
pub fn parse_quarantine(text: &str) -> Option<Quarantine> {
    let mut fields = text.trim_end_matches('\0').split(';');
    let flags = u16::from_str_radix(fields.next()?.trim(), 16).ok()?;
    let time = fields.next()?;
    let agent = fields.next()?;
    Some(Quarantine {
        flags,
        unix_seconds: u64::from_str_radix(time.trim(), 16).ok(),
        agent: agent.to_owned(),
        uuid: fields.next().map(str::to_owned),
    })
}

/// One user tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// The tag's name.
    pub name: String,
    /// The Finder colour, 0 (none) to 7: 1 Gray, 2 Green, 3 Purple, 4 Blue,
    /// 5 Yellow, 6 Red, 7 Orange.
    pub colour: u8,
}

/// The addresses in a `kMDItemWhereFroms` value, a property list array of strings.
pub fn where_froms(value: &[u8]) -> Result<Vec<String>> {
    Ok(parse_any(value)?
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.as_str())
        .filter(|text| !text.is_empty())
        .take(MAX_ITEMS)
        .map(str::to_owned)
        .collect())
}

/// The tags in a `_kMDItemUserTags` value: strings "Name" or "Name\nN", N the colour 0 to 7.
pub fn user_tags(value: &[u8]) -> Result<Vec<Tag>> {
    Ok(parse_any(value)?
        .as_array()
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item.as_str())
        .take(MAX_ITEMS)
        .map(|text| match text.rsplit_once('\n') {
            Some((name, colour)) => match colour.trim().parse::<u8>() {
                Ok(colour) if colour <= 7 => Tag {
                    name: name.to_owned(),
                    colour,
                },
                _ => Tag {
                    name: text.to_owned(),
                    colour: 0,
                },
            },
            None => Tag {
                name: text.to_owned(),
                colour: 0,
            },
        })
        .collect())
}

/// The text of a `kMDItemFinderComment` value, a property list string.
pub fn finder_comment(value: &[u8]) -> Result<Option<String>> {
    Ok(match parse_any(value)? {
        Value::String(text) if !text.is_empty() => Some(text),
        _ => None,
    })
}

/// What the Mac attributes of one node say. Missing and unreadable ones are
/// left out: this is advisory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacAttributes {
    /// The download record.
    pub quarantine: Option<Quarantine>,
    /// Where the file came from, most specific first as Apple stores them.
    pub where_from: Vec<String>,
    /// The user's tags.
    pub tags: Vec<Tag>,
    /// The Finder comment.
    pub comment: Option<String>,
}

impl MacAttributes {
    /// True when there is nothing to show.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Reads the four attributes of `node` ([`MAX_XATTR`] bytes each at most).
pub fn read_attributes(fs: &dyn FileSystem, node: NodeId) -> MacAttributes {
    let value = |name: &str| fs.read_xattr(node, name, MAX_XATTR).ok().flatten();
    MacAttributes {
        quarantine: value(QUARANTINE)
            .and_then(|bytes| parse_quarantine(&String::from_utf8_lossy(&bytes))),
        where_from: value(WHERE_FROMS)
            .and_then(|bytes| where_froms(&bytes).ok())
            .unwrap_or_default(),
        tags: value(USER_TAGS)
            .and_then(|bytes| user_tags(&bytes).ok())
            .unwrap_or_default(),
        comment: value(FINDER_COMMENT).and_then(|bytes| finder_comment(&bytes).ok().flatten()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A property list: an array of ASCII strings.
    fn string_array(items: &[&str]) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = vec![{
            let mut array = vec![0xA0 | items.len() as u8];
            array.extend((1..=items.len() as u8).collect::<Vec<_>>());
            array
        }];
        for item in items {
            let mut object = vec![0x50 | item.len() as u8];
            object.extend_from_slice(item.as_bytes());
            objects.push(object);
        }
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for object in &objects {
            offsets.push(out.len() as u8);
            out.extend_from_slice(object);
        }
        let table = out.len() as u64;
        out.extend_from_slice(&offsets);
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&[1, 1]);
        out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&table.to_be_bytes());
        out
    }

    #[test]
    fn quarantine_with_three_and_four_fields() {
        let four = parse_quarantine("0083;5f4e1b2c;Safari;3F2A-9C").unwrap();
        assert_eq!(four.flags, 0x83);
        assert_eq!(four.unix_seconds, Some(0x5f4e_1b2c));
        assert_eq!(four.agent, "Safari");
        assert_eq!(four.uuid.as_deref(), Some("3F2A-9C"));
        let three = parse_quarantine("0081;5f4e1b2c;Chrome").unwrap();
        assert_eq!(three.uuid, None);
        // A time that is not hex leaves the time out and keeps the rest.
        let odd = parse_quarantine("0083;yesterday;Safari").unwrap();
        assert_eq!(odd.unix_seconds, None);
        assert_eq!(odd.agent, "Safari");
        assert!(parse_quarantine("0083;5f4e1b2c").is_none());
        assert!(parse_quarantine("zz;1;a").is_none());
        assert!(parse_quarantine("").is_none());
    }

    #[test]
    fn tags_with_and_without_the_colour_line() {
        let tags = user_tags(&string_array(&["Work\n6", "Plain", "Odd\n9"])).unwrap();
        assert_eq!(
            tags,
            vec![
                Tag {
                    name: "Work".into(),
                    colour: 6
                },
                Tag {
                    name: "Plain".into(),
                    colour: 0
                },
                Tag {
                    name: "Odd\n9".into(),
                    colour: 0
                },
            ]
        );
    }

    #[test]
    fn origins_and_comments() {
        let froms = where_froms(&string_array(&[
            "https://a.example/f.zip",
            "https://a.example/",
        ]))
        .unwrap();
        assert_eq!(froms.len(), 2);
        // A comment is one string object.
        let mut comment = b"bplist00".to_vec();
        comment.extend_from_slice(b"\x55hello");
        let table = comment.len() as u64;
        comment.push(8);
        comment.extend_from_slice(&[0; 6]);
        comment.extend_from_slice(&[1, 1]);
        comment.extend_from_slice(&1u64.to_be_bytes());
        comment.extend_from_slice(&0u64.to_be_bytes());
        comment.extend_from_slice(&table.to_be_bytes());
        assert_eq!(finder_comment(&comment).unwrap().as_deref(), Some("hello"));
        // Garbage is an error, not a panic.
        assert!(where_froms(b"bplist00garbage").is_err());
    }
}
