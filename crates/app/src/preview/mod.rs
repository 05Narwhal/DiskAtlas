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

//! The preview pipeline behind Quick Look: turns a file's bytes into
//! something the panel can draw.
//!
//! [`load`] is the whole of it. It opens the stream, reads its first bytes
//! once, asks each [`PreviewProvider`] in order whether it wants the file,
//! lets the first that does read it, and decodes a picture if that is what
//! came back. Everything here is blocking and runs on the background
//! executor; nothing here touches an entity or a window.
//!
//! The file is hostile input. The rules that follow from that live here so
//! a provider cannot forget them:
//!
//! - A provider says how big a picture is ([`EncodedImage`]) before anything
//!   decodes it. The pipeline refuses pictures past [`MAX_IMAGE_PIXELS`] or
//!   [`MAX_DECODED_BYTES`]: a 60 KB PNG can declare 30,000 by 30,000 pixels.
//! - Decoding happens here, on the caller's background thread, and never
//!   lazily at paint. GPUI 0.2.2 would decode an `Image` on first paint with
//!   no limit of its own and with nowhere to report an error.
//! - A panic in the third-party decoder is caught and becomes an error.
//! - SVG is never decoded: it can name other files and fonts on the host.
//! - Text arrives split into lines of at most [`MAX_LINE_BYTES`], so one
//!   256 KB line (minified code) cannot make a single row that takes
//!   seconds to shape on the UI thread.
//!
//! Design: `.claude/docs/design/m5-quicklook-copy.md` §3.

use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, anyhow, bail, ensure};
use disk_atlas_lib::fs::{FileSystem, FinderCodes, NodeId, Stream};
use disk_atlas_lib::io::block::BlockSource;
use gpui::{App, ImageFormat, RenderImage, SharedString};

pub mod image;
pub mod structure;
pub mod text;

/// How much of the stream every provider gets to look at before deciding.
pub const SNIFF_BYTES: usize = 4096;

/// Most bytes of a text file the text provider reads.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Longest row of a text preview, in bytes of UTF-8. Longer lines continue
/// on the next row.
pub const MAX_LINE_BYTES: usize = 4096;

/// Largest picture file the image provider reads. A larger one is not
/// accepted and opens on the Bytes tab.
pub const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;

/// Most pixels one frame of a picture may have.
pub const MAX_IMAGE_PIXELS: u64 = 64_000_000;

/// Most bytes a decoded picture may take: four per pixel, every frame of an
/// animation counted.
pub const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// Size of one read while a provider pulls a stream into memory. The cancel
/// flag is checked between reads.
const READ_PIECE: usize = 1024 * 1024;

/// The file to preview.
#[derive(Clone)]
pub struct Subject {
    /// The filesystem the file is on.
    pub fs: Arc<dyn FileSystem>,
    /// The file.
    pub node: NodeId,
    /// Its name, for providers that weigh the extension.
    pub name: SharedString,
    /// Which of its streams to show.
    pub stream: Stream,
    /// Its classic Mac OS type and creator, when it has them.
    pub finder_codes: Option<FinderCodes>,
    /// The volume's `VolumeInfo::format` ("HFS", "HFS+"), which decides the
    /// fallback text encoding.
    pub format: Option<&'static str>,
}

/// The first bytes of the stream, read once and shown to every provider.
pub struct Sniff {
    /// Up to [`SNIFF_BYTES`] from the start of the stream.
    pub head: Vec<u8>,
    /// Length of the whole stream in bytes.
    pub len: u64,
}

/// Text ready to draw: decoded, line endings made uniform, split into rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPreview {
    text: Arc<str>,
    rows: Arc<[Range<u32>]>,
    /// The encoding the bytes were read as, for the user ("UTF-8",
    /// "Mac OS Roman").
    pub encoding: &'static str,
    /// True when the file is longer than what was read.
    pub truncated: bool,
}

impl TextPreview {
    /// Splits `text` into rows. `text` has LF line endings only (the
    /// provider has already turned CR and CRLF into LF). A line longer than
    /// [`MAX_LINE_BYTES`] is cut at a character boundary and continues on
    /// the next row.
    ///
    /// Text past 4 GiB is dropped, which no provider can reach.
    pub fn new(mut text: String, encoding: &'static str, truncated: bool) -> Self {
        if u32::try_from(text.len()).is_err() {
            let mut end = u32::MAX as usize;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let mut rows = Vec::new();
        let mut start = 0usize;
        for line in text.split('\n') {
            let end = start + line.len();
            let mut at = start;
            // An empty line is one empty row; a long one is several.
            loop {
                let mut cut = end.min(at + MAX_LINE_BYTES);
                while cut > at && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                rows.push(to_u32(at)..to_u32(cut));
                at = cut;
                if at >= end {
                    break;
                }
            }
            start = end + 1;
        }
        // "a\n" is one line, not a line and an empty one.
        if text.ends_with('\n') {
            rows.pop();
        }
        if text.is_empty() {
            rows.clear();
        }
        Self {
            text: text.into(),
            rows: rows.into(),
            encoding,
            truncated,
        }
    }

    /// Number of rows.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The text of row `index`, without its line ending.
    pub fn row(&self, index: usize) -> Option<&str> {
        let range = self.rows.get(index)?;
        self.text.get(range.start as usize..range.end as usize)
    }

    /// The whole text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Bytes this preview keeps alive, for the cache's budget.
    fn cost(&self) -> u64 {
        (self.text.len() + self.rows.len() * size_of::<Range<u32>>()) as u64
    }
}

/// `value` as `u32`. Callers pass offsets into text already capped at
/// `u32::MAX` bytes.
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A picture as a provider hands it over: still encoded, with the size its
/// own header declares.
pub struct EncodedImage {
    /// The container format. Never `Svg`.
    pub format: ImageFormat,
    /// The whole file.
    pub bytes: Vec<u8>,
    /// Width in pixels, from the file's header.
    pub width: u32,
    /// Height in pixels, from the file's header.
    pub height: u32,
    /// Frames the decoder will produce: 1 for a still picture, the number
    /// of image blocks for a GIF.
    pub frames: u32,
}

/// A decoded picture.
#[derive(Clone)]
pub struct ImagePreview {
    /// The pixels, ready for `img(ImageSource::Render(..))`.
    ///
    /// GPUI keeps a texture for it once it is painted; whoever drops the
    /// last preview of a picture calls `App::drop_image` with this.
    pub image: Arc<RenderImage>,
    /// Width in pixels, as decoded.
    pub width: u32,
    /// Height in pixels, as decoded.
    pub height: u32,
    /// Frames decoded (more than one for an animated GIF).
    pub frames: usize,
}

impl ImagePreview {
    fn cost(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4 * self.frames as u64
    }
}

/// What a provider made of the file.
pub enum Content {
    /// Text to show.
    Text(TextPreview),
    /// A picture for the pipeline to check and decode.
    Image(EncodedImage),
}

/// What the Preview tab shows.
#[derive(Clone)]
pub enum Preview {
    /// Text.
    Text(TextPreview),
    /// A picture.
    Image(ImagePreview),
    /// No provider wanted the file. The panel opens on the Bytes tab.
    None,
}

impl Preview {
    /// Bytes this preview keeps alive, for the cache's budget.
    pub fn cost(&self) -> u64 {
        match self {
            Self::Text(text) => text.cost(),
            Self::Image(image) => image.cost(),
            Self::None => 0,
        }
    }
}

/// One kind of preview. Implementations are plain blocking code with no
/// state: they are tested with byte fixtures and no window.
pub trait PreviewProvider: Send + Sync {
    /// A short name for logs and tests.
    fn name(&self) -> &'static str;

    /// Whether this provider wants the file. Cheap and pure: decides from
    /// the name, the type code and the sniffed bytes, and reads nothing.
    fn accepts(&self, subject: &Subject, sniff: &Sniff) -> bool;

    /// Reads the file and builds its content. Only called after `accepts`
    /// said yes.
    ///
    /// Reads through [`read_prefix`], which checks `cancel`; a cancelled
    /// read is `Ok(None)`. An `Err` becomes the panel's message, so it says
    /// what is wrong with the file in the user's words.
    fn load(
        &self,
        subject: &Subject,
        sniff: &Sniff,
        source: &dyn BlockSource,
        cancel: &AtomicBool,
    ) -> anyhow::Result<Option<Content>>;
}

/// The providers, in the order they are asked. Pictures go first: they are
/// recognised by signature, while "looks like text" is a guess.
pub fn providers() -> &'static [&'static dyn PreviewProvider] {
    &[&image::ImageProvider, &text::TextProvider]
}

/// Turns an encoded picture into pixels. Blocking.
pub type Decoder = dyn Fn(gpui::Image) -> anyhow::Result<Arc<RenderImage>> + Send + Sync;

/// GPUI's own decoder, for [`load`]. Make one on the UI thread and move it
/// into the background task.
///
/// It is a closure because `Image::to_image_data` wants the app's SVG
/// renderer whatever the format, and GPUI 0.2.2 does not export that type's
/// name: it can be held and passed on, not written down.
pub fn decoder(cx: &App) -> Arc<Decoder> {
    let svg = cx.svg_renderer();
    Arc::new(move |image| image.to_image_data(svg.clone()))
}

/// Builds the preview of `subject`. Blocking: call it on the background
/// executor, with the [`decoder`] made beforehand.
///
/// `Ok(None)` when `cancel` was set before the preview was complete. `Err`
/// when the file could not be read or decoded; the text is for the user.
pub fn load(
    subject: &Subject,
    decoder: &Decoder,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<Preview>> {
    load_with(
        providers(),
        subject,
        &|encoded| decode(encoded, decoder),
        cancel,
    )
}

/// [`load`] with the providers and the decoder passed in, for tests.
fn load_with(
    providers: &[&dyn PreviewProvider],
    subject: &Subject,
    decode: &dyn Fn(EncodedImage) -> anyhow::Result<ImagePreview>,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<Preview>> {
    // A resource fork has no preview in M5: it opens on the Bytes tab,
    // where the structure notes explain its header.
    if subject.stream != Stream::Data {
        return Ok(Some(Preview::None));
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let source = subject.fs.open(subject.node, subject.stream)?;
    let len = source.len();
    if len == 0 {
        return Ok(Some(Preview::Text(TextPreview::new(
            String::new(),
            "UTF-8",
            false,
        ))));
    }
    let want = usize::try_from(len).unwrap_or(usize::MAX).min(SNIFF_BYTES);
    let mut head = vec![0u8; want];
    source.read_at(0, &mut head)?;
    let sniff = Sniff { head, len };

    let Some(provider) = providers.iter().find(|p| p.accepts(subject, &sniff)) else {
        return Ok(Some(Preview::None));
    };
    tracing::debug!(provider = provider.name(), len, "preview: provider chosen");
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let Some(content) = provider.load(subject, &sniff, source.as_ref(), cancel)? else {
        return Ok(None);
    };
    let preview = match content {
        Content::Text(text) => Preview::Text(text),
        Content::Image(encoded) => {
            check_image(&encoded)?;
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            Preview::Image(decode(encoded)?)
        }
    };
    // A decode is the one step that cannot be interrupted; its result is
    // still dropped if the user moved on meanwhile.
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    Ok(Some(preview))
}

/// Reads the first `limit` bytes of `source` (all of it when it is
/// shorter) in pieces, checking `cancel` between them. `Ok(None)` when
/// cancelled.
///
/// Memory grows with what was actually read, so a stream that claims a
/// length it cannot deliver fails at the first bad read instead of
/// allocating its claim.
pub fn read_prefix(
    source: &dyn BlockSource,
    limit: usize,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<Vec<u8>>> {
    let total = usize::try_from(source.len())
        .unwrap_or(usize::MAX)
        .min(limit);
    let mut bytes = Vec::new();
    while bytes.len() < total {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let start = bytes.len();
        let piece = (total - start).min(READ_PIECE);
        bytes.resize(start + piece, 0);
        source.read_bulk(start as u64, &mut bytes[start..])?;
    }
    Ok(Some(bytes))
}

/// Refuses a picture whose declared size is past the limits, before a
/// single pixel is decoded.
fn check_image(encoded: &EncodedImage) -> anyhow::Result<()> {
    ensure!(
        encoded.format != ImageFormat::Svg,
        "SVG pictures are not previewed."
    );
    ensure!(
        encoded.width > 0 && encoded.height > 0 && encoded.frames > 0,
        "This picture says it is empty."
    );
    let pixels = u64::from(encoded.width) * u64::from(encoded.height);
    ensure!(
        pixels <= MAX_IMAGE_PIXELS,
        "This picture is too large to preview ({} × {} pixels).",
        encoded.width,
        encoded.height
    );
    // No overflow: pixels <= 64e6, times 4, times a u32.
    let decoded = pixels * 4 * u64::from(encoded.frames);
    ensure!(
        decoded <= MAX_DECODED_BYTES,
        "This animation is too large to preview ({} frames of {} × {} pixels).",
        encoded.frames,
        encoded.width,
        encoded.height
    );
    Ok(())
}

/// Decodes a checked picture.
fn decode(encoded: EncodedImage, decoder: &Decoder) -> anyhow::Result<ImagePreview> {
    let image = gpui::Image::from_bytes(encoded.format, encoded.bytes);
    // The decoder is third-party code (the `image` crate) reading hostile
    // bytes. It is written to return errors, but a panic there must not
    // take the app down.
    let decoded = catch_unwind(AssertUnwindSafe(move || decoder(image)))
        .map_err(|_| anyhow!("This picture could not be decoded (the decoder failed)."))?
        .context("This picture could not be decoded")?;
    let frames = decoded.frame_count();
    if frames == 0 {
        bail!("This picture has no frames.");
    }
    // `size` indexes the frames; there is at least one.
    let size = decoded.size(0);
    let width = u32::try_from(size.width.0).unwrap_or(0);
    let height = u32::try_from(size.height.0).unwrap_or(0);
    Ok(ImagePreview {
        image: decoded,
        width,
        height,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use disk_atlas_lib::error::{Error, Result};
    use disk_atlas_lib::fs::{DirStream, Metadata, VolumeInfo};
    use disk_atlas_lib::io::block::MemSource;

    use super::*;

    /// A volume of one file, node 5, with the given content.
    struct OneFile {
        bytes: Vec<u8>,
        opens: AtomicUsize,
    }

    impl OneFile {
        fn subject(bytes: &[u8]) -> Subject {
            Subject {
                fs: Arc::new(Self {
                    bytes: bytes.to_vec(),
                    opens: AtomicUsize::new(0),
                }),
                node: NodeId(5),
                name: "file".into(),
                stream: Stream::Data,
                finder_codes: None,
                format: None,
            }
        }
    }

    impl FileSystem for OneFile {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("one")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(Error::NotFound { node: node.0 })
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            Err(Error::NotADirectory { node: dir.0 })
        }
        fn open(&self, node: NodeId, _stream: Stream) -> Result<Arc<dyn BlockSource>> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            if node != NodeId(5) {
                return Err(Error::NotFound { node: node.0 });
            }
            Ok(Arc::new(MemSource::new(self.bytes.clone())))
        }
    }

    /// Accepts files starting with `magic` and answers with `make`.
    struct Fake {
        magic: &'static [u8],
        make: fn(Vec<u8>) -> anyhow::Result<Content>,
    }

    impl PreviewProvider for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn accepts(&self, _subject: &Subject, sniff: &Sniff) -> bool {
            sniff.head.starts_with(self.magic)
        }
        fn load(
            &self,
            _subject: &Subject,
            _sniff: &Sniff,
            source: &dyn BlockSource,
            cancel: &AtomicBool,
        ) -> anyhow::Result<Option<Content>> {
            let Some(bytes) = read_prefix(source, usize::MAX, cancel)? else {
                return Ok(None);
            };
            (self.make)(bytes).map(Some)
        }
    }

    fn as_text(bytes: Vec<u8>) -> anyhow::Result<Content> {
        Ok(Content::Text(TextPreview::new(
            String::from_utf8(bytes)?,
            "UTF-8",
            false,
        )))
    }

    fn as_image(bytes: Vec<u8>) -> anyhow::Result<Content> {
        Ok(Content::Image(EncodedImage {
            format: ImageFormat::Bmp,
            width: u32::from(bytes[1]),
            height: u32::from(bytes[2]),
            frames: u32::from(bytes[3]),
            bytes,
        }))
    }

    const TEXT: Fake = Fake {
        magic: b"T",
        make: as_text,
    };
    const IMAGE: Fake = Fake {
        magic: b"I",
        make: as_image,
    };

    fn no_decode(_: EncodedImage) -> anyhow::Result<ImagePreview> {
        panic!("nothing should be decoded here")
    }

    fn run(bytes: &[u8]) -> anyhow::Result<Option<Preview>> {
        load_with(
            &[&IMAGE, &TEXT],
            &OneFile::subject(bytes),
            &no_decode,
            &AtomicBool::new(false),
        )
    }

    fn rows(text: &str) -> Vec<String> {
        let preview = TextPreview::new(text.to_owned(), "UTF-8", false);
        (0..preview.row_count())
            .map(|i| preview.row(i).unwrap().to_owned())
            .collect()
    }

    #[test]
    fn text_splits_into_rows() {
        assert_eq!(rows(""), Vec::<String>::new());
        assert_eq!(rows("a"), ["a"]);
        assert_eq!(rows("a\n"), ["a"]);
        assert_eq!(rows("a\nb"), ["a", "b"]);
        assert_eq!(rows("a\n\nb\n"), ["a", "", "b"]);
        assert_eq!(rows("\n"), [""]);
        assert_eq!(rows("\n\n"), ["", ""]);
        assert_eq!(rows("é\nü"), ["é", "ü"]);
    }

    #[test]
    fn long_lines_continue_on_the_next_row() {
        // 'é' is two bytes, so the cut cannot fall at exactly MAX_LINE_BYTES
        // on every row: it backs up to a character boundary.
        let line = "aé".repeat(MAX_LINE_BYTES);
        let all = rows(&format!("{line}\nshort"));
        assert!(all.len() > 3);
        let (last, long) = all.split_last().unwrap();
        assert_eq!(last, "short");
        assert!(
            long.iter()
                .all(|r| !r.is_empty() && r.len() <= MAX_LINE_BYTES)
        );
        assert_eq!(long.concat(), line);
    }

    #[test]
    fn row_out_of_range_is_none() {
        let preview = TextPreview::new("a\nb".to_owned(), "UTF-8", true);
        assert_eq!(preview.row(2), None);
        assert_eq!(preview.text(), "a\nb");
        assert!(preview.truncated);
        assert!(preview.cost() >= 3);
    }

    #[test]
    fn first_accepting_provider_wins() {
        let Some(Preview::Text(text)) = run(b"Thello\nworld").unwrap() else {
            panic!("expected text");
        };
        assert_eq!(text.row(1), Some("world"));
    }

    #[test]
    fn nobody_accepting_is_no_preview() {
        assert!(matches!(run(b"\x00\x01\x02").unwrap(), Some(Preview::None)));
    }

    #[test]
    fn empty_stream_is_empty_text() {
        let Some(Preview::Text(text)) = run(b"").unwrap() else {
            panic!("expected text");
        };
        assert_eq!(text.row_count(), 0);
    }

    #[test]
    fn resource_fork_has_no_preview_and_is_not_opened() {
        let mut subject = OneFile::subject(b"Thello");
        subject.stream = Stream::Resource;
        let out = load_with(&[&TEXT], &subject, &no_decode, &AtomicBool::new(false)).unwrap();
        assert!(matches!(out, Some(Preview::None)));
    }

    #[test]
    fn open_failure_is_an_error() {
        let mut subject = OneFile::subject(b"Thello");
        subject.node = NodeId(9);
        assert!(load_with(&[&TEXT], &subject, &no_decode, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn provider_error_is_an_error() {
        // Not UTF-8, so the fake text provider fails.
        assert!(run(b"T\xFF\xFE").is_err());
    }

    #[test]
    fn cancelled_before_start_reads_nothing() {
        let subject = OneFile::subject(b"Thello");
        let out = load_with(&[&TEXT], &subject, &no_decode, &AtomicBool::new(true)).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn oversized_pictures_are_refused_before_decoding() {
        let encoded = |width, height, frames| EncodedImage {
            format: ImageFormat::Png,
            bytes: Vec::new(),
            width,
            height,
            frames,
        };
        assert!(check_image(&encoded(8000, 8000, 1)).is_ok());
        assert!(check_image(&encoded(8001, 8000, 1)).is_err());
        assert!(check_image(&encoded(30_000, 30_000, 1)).is_err());
        assert!(check_image(&encoded(u32::MAX, u32::MAX, u32::MAX)).is_err());
        assert!(check_image(&encoded(0, 10, 1)).is_err());
        assert!(check_image(&encoded(10, 10, 0)).is_err());
        // 1000 x 1000 x 4 bytes = 4 MB a frame: 67 frames fit, 68 do not.
        assert!(check_image(&encoded(1000, 1000, 67)).is_ok());
        assert!(check_image(&encoded(1000, 1000, 68)).is_err());
        let svg = EncodedImage {
            format: ImageFormat::Svg,
            ..encoded(10, 10, 1)
        };
        assert!(check_image(&svg).is_err());

        // Through the pipeline: the refusal comes before the decoder, which
        // here would panic.
        let refused = load_with(
            &[&Fake {
                magic: b"I",
                make: |bytes| {
                    Ok(Content::Image(EncodedImage {
                        format: ImageFormat::Bmp,
                        bytes,
                        width: 40_000,
                        height: 40_000,
                        frames: 1,
                    }))
                },
            }],
            &OneFile::subject(b"I"),
            &no_decode,
            &AtomicBool::new(false),
        );
        assert!(refused.is_err());
    }

    #[test]
    fn read_prefix_stops_at_the_limit_and_at_cancel() {
        let source = MemSource::new(vec![7u8; 3 * READ_PIECE + 5]);
        let go = AtomicBool::new(false);
        let all = read_prefix(&source, usize::MAX, &go).unwrap().unwrap();
        assert_eq!(all.len(), 3 * READ_PIECE + 5);
        assert!(all.iter().all(|&b| b == 7));
        let some = read_prefix(&source, READ_PIECE + 1, &go).unwrap().unwrap();
        assert_eq!(some.len(), READ_PIECE + 1);
        assert!(
            read_prefix(&source, 0, &go)
                .unwrap()
                .is_some_and(|b| b.is_empty())
        );
        let stop = AtomicBool::new(true);
        assert!(read_prefix(&source, usize::MAX, &stop).unwrap().is_none());
    }

    // GPUI's real decoder needs the app's SVG renderer, which only a
    // running `App` can give, so these cover what surrounds it. The decode
    // of a real picture is checked in the window (M5 task 8).

    fn tiny() -> EncodedImage {
        EncodedImage {
            format: ImageFormat::Png,
            bytes: b"not a picture".to_vec(),
            width: 1,
            height: 1,
            frames: 1,
        }
    }

    #[test]
    fn a_decoder_error_is_an_error() {
        let failing: &Decoder = &|_| Err(anyhow!("bad header"));
        let Err(error) = decode(tiny(), failing) else {
            panic!("expected an error");
        };
        // The user's sentence first, the decoder's reason kept behind it.
        assert_eq!(error.to_string(), "This picture could not be decoded");
        assert_eq!(error.root_cause().to_string(), "bad header");
    }

    #[test]
    fn a_decoder_panic_is_an_error_not_a_crash() {
        let panicking: &Decoder = &|_| panic!("decoder bug");
        let Err(error) = decode(tiny(), panicking) else {
            panic!("expected an error");
        };
        assert!(error.to_string().contains("the decoder failed"));
    }

    #[test]
    fn the_decoder_gets_the_bytes_and_format_unchanged() {
        let seen: &Decoder = &|image| {
            assert_eq!(image.format, ImageFormat::Png);
            assert_eq!(image.bytes, b"not a picture");
            Err(anyhow!("stop here"))
        };
        assert!(decode(tiny(), seen).is_err());
    }
}
