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

//! Thumbnails for the file list and the icon view (`r1-redesign.md` 1.7).
//!
//! The views say which rows are on screen ([`Thumbnails::want`]); nothing is read until the
//! scrolling has stopped for [`SETTLE`]. Pictures are decoded and shrunk on the background
//! executor, at most [`THUMB_JOBS`] at a time, and kept in a cache bounded by [`THUMB_CACHE`]
//! entries and [`THUMB_BYTES`] bytes, least recently used out first. A file that cannot be
//! shown (damaged, too large, not a picture after all) is remembered and not tried again.
//!
//! Pictures and custom icons are done here (R17b): a folder's `Icon\r` and a package's
//! `CFBundleIconFile`. A volume's `.VolumeIcon.icns` waits for icons in the sidebar. A custom icon that cannot be read or
//! decoded is remembered as absent and the row keeps its kind's glyph, without a message. Other
//! kinds keep their glyph; PDF, fonts and 3D models are later work on the same pipeline.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use disk_atlas_lib::fs::{FileSystem, NodeId, NodeKind, Stream};
use disk_atlas_lib::mac::bundle;
use disk_atlas_lib::mac::icns::{Icns, IconImage, MAX_ICNS};
use gpui::{Context, RenderImage, SharedString, Task};
use image::{DynamicImage, Frame, ImageFormat, ImageReader, Limits, imageops::FilterType};

use super::sources::SourceId;
use crate::preview::{MAX_IMAGE_FILE, read_prefix};

/// Pictures decoded at the same time.
pub const THUMB_JOBS: usize = 4;
/// Most thumbnails kept.
pub const THUMB_CACHE: usize = 512;
/// Most pixel bytes kept (48 MiB).
pub const THUMB_BYTES: u64 = 48 * 1024 * 1024;
/// How long the wanted set must stay unchanged before decoding starts.
pub const SETTLE: Duration = Duration::from_millis(60);
/// Pictures with more pixels than this are not thumbnailed: the decode alone would be 100 MB.
const MAX_PIXELS: u64 = 24_000_000;
/// Most memory the decoder may allocate for one picture.
const MAX_ALLOC: u64 = 160 * 1024 * 1024;

/// What a thumbnail is of: the file, the edge it was made for and the time it was modified, so a
/// changed file gets a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub source: SourceId,
    pub node: NodeId,
    /// Longest side, 64, 128 or 256.
    pub edge: u32,
    pub modified: Option<i64>,
}

/// Where a thumbnail comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    /// The file is a picture.
    #[default]
    Picture,
    /// A folder's own icon: the `icns` resource of its `Icon\r` file.
    FolderIcon,
    /// A package's icon: `CFBundleIconFile` of `Contents/Info.plist`.
    PackageIcon,
}

/// One file the views want a thumbnail of.
#[derive(Clone)]
pub struct Want {
    pub key: Key,
    pub fs: Arc<dyn FileSystem>,
    pub name: SharedString,
    pub origin: Origin,
}

/// The longest side to make for a tile or glyph box `side` pixels across.
pub fn edge_for(side: f32) -> u32 {
    if side <= 40.0 {
        64
    } else if side <= 100.0 {
        128
    } else {
        256
    }
}

/// True when `name` and its size say the file is a picture worth asking for.
pub fn eligible(name: &str, kind: NodeKind, size: Option<u64>) -> bool {
    kind == NodeKind::File
        && size.is_some_and(|size| size > 0 && size <= MAX_IMAGE_FILE)
        && name
            .rsplit_once('.')
            .is_some_and(|(_, extension)| format_of(extension).is_some())
}

fn format_of(extension: &str) -> Option<ImageFormat> {
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" | "jpe" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "webp" => Some(ImageFormat::WebP),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

enum Slot {
    /// A decode is queued or running.
    Pending,
    /// Tried once, cannot be made.
    Absent,
    Ready {
        image: Arc<RenderImage>,
        bytes: u64,
        used: u64,
    },
}

/// The thumbnail cache and the work that fills it.
pub struct Thumbnails {
    slots: HashMap<Key, Slot>,
    /// Counts uses, for least-recently-used.
    clock: u64,
    bytes: u64,
    /// Wanted and not started.
    queue: VecDeque<Want>,
    running: usize,
    settle: Option<Task<()>>,
}

impl Thumbnails {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            clock: 0,
            bytes: 0,
            queue: VecDeque::new(),
            running: 0,
            settle: None,
        }
    }

    /// The thumbnail for `key`, when it is made.
    pub fn image(&self, key: &Key) -> Option<Arc<RenderImage>> {
        match self.slots.get(key) {
            Some(Slot::Ready { image, .. }) => Some(Arc::clone(image)),
            _ => None,
        }
    }

    /// The views' answer to "what is on screen now": replaces what was waiting, marks what is
    /// cached as used and, once the set has stayed the same for [`SETTLE`], decodes the rest.
    pub fn want(&mut self, wants: Vec<Want>, cx: &mut Context<Self>) {
        // Entries whose job never started are off screen now: forgotten, so they can be asked
        // for again.
        for stale in self.queue.drain(..) {
            if matches!(self.slots.get(&stale.key), Some(Slot::Pending)) {
                self.slots.remove(&stale.key);
            }
        }
        for want in wants {
            self.clock += 1;
            match self.slots.get_mut(&want.key) {
                Some(Slot::Ready { used, .. }) => *used = self.clock,
                Some(Slot::Pending | Slot::Absent) => {}
                None => {
                    self.slots.insert(want.key, Slot::Pending);
                    self.queue.push_back(want);
                }
            }
        }
        if self.queue.is_empty() {
            self.settle = None;
            return;
        }
        self.settle = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |this, cx| this.pump(cx)).ok();
        }));
    }

    /// Starts as many jobs as there are free slots.
    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.running < THUMB_JOBS {
            let Some(want) = self.queue.pop_front() else {
                break;
            };
            self.running += 1;
            let key = want.key;
            cx.spawn(async move |this, cx| {
                let made = cx
                    .background_executor()
                    .spawn(async move { make(&want) })
                    .await;
                this.update(cx, |this, cx| this.finish(key, made, cx)).ok();
            })
            .detach();
        }
    }

    fn finish(&mut self, key: Key, made: Option<(Arc<RenderImage>, u64)>, cx: &mut Context<Self>) {
        self.running = self.running.saturating_sub(1);
        match made {
            Some((image, bytes)) => {
                self.clock += 1;
                self.bytes += bytes;
                self.slots.insert(
                    key,
                    Slot::Ready {
                        image,
                        bytes,
                        used: self.clock,
                    },
                );
                self.evict(cx);
                cx.notify();
            }
            None => {
                self.slots.insert(key, Slot::Absent);
            }
        }
        self.pump(cx);
    }

    /// Drops least recently used thumbnails until the cache is inside its limits, and their
    /// textures with them.
    fn evict(&mut self, cx: &mut Context<Self>) {
        loop {
            let ready = self
                .slots
                .values()
                .filter(|slot| matches!(slot, Slot::Ready { .. }))
                .count();
            if ready <= THUMB_CACHE && self.bytes <= THUMB_BYTES {
                return;
            }
            let oldest = self
                .slots
                .iter()
                .filter_map(|(key, slot)| match slot {
                    Slot::Ready { used, .. } => Some((*used, *key)),
                    _ => None,
                })
                .min_by_key(|(used, _)| *used);
            let Some((_, key)) = oldest else {
                return;
            };
            if let Some(Slot::Ready { image, bytes, .. }) = self.slots.remove(&key) {
                self.bytes = self.bytes.saturating_sub(bytes);
                cx.drop_image(image, None);
            }
        }
    }
}

/// Reads, decodes and shrinks one picture. Blocking; `None` for anything that cannot be shown.
fn make(want: &Want) -> Option<(Arc<RenderImage>, u64)> {
    if want.origin != Origin::Picture {
        let icns = custom_icns(want)?;
        let edge = want.key.edge;
        let small = match icns.best(edge)? {
            IconImage::Png(bytes) => shrink(&bytes, ImageFormat::Png, edge)?,
            IconImage::Rgba(rgba) => {
                let image = image::RgbaImage::from_raw(rgba.width, rgba.height, rgba.pixels)?;
                if image.width() > edge || image.height() > edge {
                    DynamicImage::ImageRgba8(image)
                        .resize(edge, edge, FilterType::Triangle)
                        .into_rgba8()
                } else {
                    image
                }
            }
        };
        return render_image(small);
    }
    let format = format_of(want.name.rsplit_once('.')?.1)?;
    let source = want.fs.open(want.key.node, Stream::Data).ok()?;
    let never = AtomicBool::new(false);
    let limit = usize::try_from(MAX_IMAGE_FILE).ok()?;
    let bytes = read_prefix(source.as_ref(), limit, &never).ok()??;
    let small = shrink(&bytes, format, want.key.edge)?;
    render_image(small)
}

/// The icon file of a custom-icon `want`, parsed. Blocking; `None` for anything missing,
/// unreadable or damaged.
fn custom_icns(want: &Want) -> Option<Icns> {
    let fs = want.fs.as_ref();
    let never = AtomicBool::new(false);
    let node = want.key.node;
    match want.origin {
        Origin::Picture => None,
        Origin::FolderIcon => {
            let icon = fs.lookup(node, "Icon\r").ok()??;
            let fork = fs.open(icon, Stream::Resource).ok()?;
            Icns::from_resource_fork(fork.as_ref()).ok()?
        }
        Origin::PackageIcon => {
            let contents = fs.lookup(node, "Contents").ok()??;
            let plist = fs.lookup(contents, "Info.plist").ok()??;
            let source = fs.open(plist, Stream::Data).ok()?;
            // One byte over the limit is read, so that a bigger file is refused, not cut.
            let bytes = read_prefix(source.as_ref(), bundle::MAX_INFO_PLIST + 1, &never).ok()??;
            let name = bundle::icon_file_name(&bytes)?;
            let resources = fs.lookup(contents, "Resources").ok()??;
            let file = fs.lookup(resources, &name).ok()??;
            read_icns(fs, file, &never)
        }
    }
}

fn read_icns(fs: &dyn FileSystem, file: NodeId, never: &AtomicBool) -> Option<Icns> {
    let source = fs.open(file, Stream::Data).ok()?;
    if source.len() > MAX_ICNS as u64 {
        return None;
    }
    let bytes = read_prefix(source.as_ref(), MAX_ICNS, never).ok()??;
    Icns::parse(&bytes).ok()
}

/// Turns a shrunk picture into the texture GPUI draws, and its cost in bytes.
fn render_image(small: image::RgbaImage) -> Option<(Arc<RenderImage>, u64)> {
    let (width, height) = (small.width(), small.height());
    let mut pixels = small.into_raw();
    // GPUI draws BGRA.
    for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
    let cost = u64::from(width) * u64::from(height) * 4;
    Some((Arc::new(RenderImage::new(vec![Frame::new(buffer)])), cost))
}

/// Decodes `bytes` and shrinks the picture to at most `edge` on its longest side. `None` for a
/// picture that is damaged, or has more than [`MAX_PIXELS`] pixels.
fn shrink(bytes: &[u8], format: ImageFormat, edge: u32) -> Option<image::RgbaImage> {
    // The size comes from the header, before any pixel is decoded.
    let (width, height) = ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .ok()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let decoded: DynamicImage = reader.decode().ok()?;
    let fitted = if decoded.width() > edge || decoded.height() > edge {
        decoded.resize(edge, edge, FilterType::Triangle)
    } else {
        decoded
    };
    Some(fitted.into_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
            .write_to(&mut out, ImageFormat::Png)
            .expect("encodes");
        out.into_inner()
    }

    #[test]
    fn a_large_picture_is_shrunk_to_the_edge_keeping_its_shape() {
        let small = shrink(&png(400, 100), ImageFormat::Png, 128).expect("decodes");
        assert_eq!((small.width(), small.height()), (128, 32));
        // A picture already inside the edge is not enlarged.
        let kept = shrink(&png(20, 10), ImageFormat::Png, 128).expect("decodes");
        assert_eq!((kept.width(), kept.height()), (20, 10));
    }

    #[test]
    fn damaged_or_wrong_bytes_give_nothing() {
        let mut bytes = png(64, 64);
        bytes.truncate(bytes.len() / 2);
        assert!(shrink(&bytes, ImageFormat::Png, 64).is_none());
        assert!(shrink(b"not a picture", ImageFormat::Jpeg, 64).is_none());
        assert!(shrink(&[], ImageFormat::Png, 64).is_none());
    }

    #[test]
    fn only_picture_files_are_asked_for() {
        let file = NodeKind::File;
        assert!(eligible("holiday.JPG", file, Some(10)));
        assert!(eligible("a.b.png", file, Some(10)));
        assert!(!eligible("notes.txt", file, Some(10)));
        assert!(!eligible("png", file, Some(10)));
        assert!(!eligible("empty.png", file, Some(0)));
        assert!(!eligible("huge.png", file, Some(MAX_IMAGE_FILE + 1)));
        assert!(!eligible("folder.png", NodeKind::Directory, Some(10)));
        assert!(!eligible("unknown-size.png", file, None));
    }

    mod custom {
        use super::*;
        use disk_atlas_lib::error::{Error, Result};
        use disk_atlas_lib::fs::native::NativeFs;
        use disk_atlas_lib::fs::{DirStream, Metadata, VolumeInfo};
        use disk_atlas_lib::io::block::{BlockSource, check_range};

        struct Bytes(Vec<u8>);

        impl BlockSource for Bytes {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn sector_size(&self) -> u32 {
                512
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
                check_range(offset, buf.len(), self.len())?;
                let start = offset as usize;
                buf.copy_from_slice(&self.0[start..start + buf.len()]);
                Ok(())
            }
        }

        /// Folders by name, files by node.
        #[derive(Default)]
        struct Tree {
            names: HashMap<(u64, String), u64>,
            files: HashMap<u64, Vec<u8>>,
        }

        impl Tree {
            fn add(&mut self, parent: u64, name: &str, node: u64, data: Option<Vec<u8>>) {
                self.names.insert((parent, name.to_owned()), node);
                if let Some(data) = data {
                    self.files.insert(node, data);
                }
            }
        }

        impl FileSystem for Tree {
            fn info(&self) -> VolumeInfo {
                NativeFs::new(std::env::temp_dir(), "test")
                    .expect("temp dir opens")
                    .info()
            }
            fn root(&self) -> NodeId {
                NodeId(1)
            }
            fn metadata(&self, node: NodeId) -> Result<Metadata> {
                Err(Error::NotFound { node: node.0 })
            }
            fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
                Ok(self
                    .names
                    .get(&(dir.0, name.to_owned()))
                    .map(|n| NodeId(*n)))
            }
            fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
                Err(Error::NotADirectory { node: dir.0 })
            }
            fn open(&self, node: NodeId, _stream: Stream) -> Result<Arc<dyn BlockSource>> {
                match self.files.get(&node.0) {
                    Some(data) => Ok(Arc::new(Bytes(data.clone()))),
                    None => Err(Error::NotFound { node: node.0 }),
                }
            }
        }

        fn icns() -> Vec<u8> {
            let png = png(32, 32);
            let mut body = b"ic08".to_vec();
            body.extend_from_slice(&(png.len() as u32 + 8).to_be_bytes());
            body.extend(png);
            let mut out = b"icns".to_vec();
            out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
            out.extend(body);
            out
        }

        fn plist(icon: &str) -> Vec<u8> {
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIconFile</key>                 <string>{icon}</string></dict></plist>"
            )
            .into_bytes()
        }

        /// A package at node 10 with its plist and an icon `App.icns`.
        fn package(info: Vec<u8>) -> Want {
            let mut tree = Tree::default();
            tree.add(10, "Contents", 11, None);
            tree.add(11, "Info.plist", 12, Some(info));
            tree.add(11, "Resources", 13, None);
            tree.add(13, "App.icns", 14, Some(icns()));
            Want {
                key: Key {
                    source: SourceId::for_test(1),
                    node: NodeId(10),
                    edge: 64,
                    modified: None,
                },
                fs: Arc::new(tree),
                name: SharedString::from("Thing.app"),
                origin: Origin::PackageIcon,
            }
        }

        #[test]
        fn a_package_icon_is_found_through_its_info_plist() {
            let (_, cost) = make(&package(plist("App"))).expect("icon is made");
            assert_eq!(cost, 32 * 32 * 4);
        }

        #[test]
        fn a_bad_plist_or_icon_name_gives_the_glyph() {
            assert!(make(&package(plist("Other"))).is_none());
            assert!(make(&package(plist("../App"))).is_none());
            assert!(make(&package(plist("Resources/App"))).is_none());
            let mut big = plist("App");
            big.resize(2 << 20, b' ');
            assert!(make(&package(big)).is_none());
            assert!(make(&package(b"junk".to_vec())).is_none());
        }

        #[test]
        fn a_folder_without_an_icon_file_gives_the_glyph() {
            let mut want = package(plist("App"));
            want.origin = Origin::FolderIcon;
            assert!(make(&want).is_none());
        }
    }

    #[test]
    fn edges_follow_the_box() {
        assert_eq!(edge_for(24.0), 64);
        assert_eq!(edge_for(80.0), 128);
        assert_eq!(edge_for(112.0), 256);
    }
}
