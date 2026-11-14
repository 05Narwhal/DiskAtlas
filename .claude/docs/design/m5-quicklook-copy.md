# M5. Quick Look and copying out

Goal: Space shows a file's content (as text, as an image, as bytes, with its details) in under 100 ms; "Copy to this computer" writes files and folders to the host, safely named, with their forks when asked.

## 1. Decisions

1. **File content is an `Arc<dyn BlockSource>`.** No separate `ReadAt` trait. `HfsPlus::open_data` already returns one, it has exactly the needed contract (length, positional thread-safe reads, a bulk path), and decmpfs and fork sources already implement it. `sector_size()` of a file stream is 512 and means nothing.
2. **`Stream` keeps two variants.** Extended attributes are small named values, not seekable streams: they get `xattrs` and `read_xattr`. This avoids a lifetime on `Stream` and a change to `extents`.
3. **Previews are an app concern** (`crates/app/src/preview/`), built on the engine's `open`. The engine never decodes a picture. Providers are plain blocking code with no GPUI types except the final image handle, so they are testable.
4. **The bytes view shows a 4 MiB window** of the stream at a time. GPUI positions are `f32` pixels; a list of more than about 800,000 rows loses pixel precision, so a multi-gigabyte file cannot be one list.
5. **Copying never overwrites and never asks.** Files are created with `create_new`. A taken name gets " (2)". (README decisions 4–6.)
6. **Name sanitizing is a pure function of the name and a rule set**, so Windows rules are tested on Linux and the reverse.
7. No new dependencies. GPUI decodes PNG, JPEG, GIF, WebP, BMP, TIFF and SVG itself (`gpui::Image::from_bytes`, `platform.rs:1702`).

## 2. Trait additions (`fs/mod.rs`) — lead

**Done (Fable, 2026-11-12).** `open`, `xattrs`, `read_xattr`, `XattrInfo`, `Metadata::finder_info` and `VolumeInfo::local_times` are in `fs/mod.rs` with defaults (`Unsupported`, empty, `None`), so every existing filesystem still compiles and task 3a only adds the real implementations. The doc comments there are the contract; the sketch below is kept for reading. One rule was added to `open`: it may read metadata but never content, so a damaged file fails in `read_at` at the damage and its readable part stays readable. `HfsPlus` and `NativeFs` set `finder_info: None` and `local_times: false` for now. The M7 trait methods (`snapshots`, `open_snapshot`) went in at the same time.

```rust
/// One extended attribute of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrInfo { pub name: String, pub size: u64 }

pub trait FileSystem {
    /// The content of `stream` of `node` as a readable source.
    ///
    /// `Stream::Data` of a transparently compressed file is the decompressed
    /// content. Hard links give their target. A symbolic link gives the
    /// stored target path. `Stream::Resource` fails with `NoSuchStream` where
    /// the format has no forks, where the fork is empty, and for a compressed
    /// file (its fork is storage, not a user's fork).
    /// `IsADirectory` for a folder. Default: `Unsupported`.
    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>>;

    /// Names and sizes of the node's extended attributes, format-internal
    /// ones left out. Default: empty.
    fn xattrs(&self, node: NodeId) -> Result<Vec<XattrInfo>>;

    /// The value of one attribute, whole. `Ok(None)` when there is no such
    /// attribute; `Unsupported` when it is longer than `max` bytes.
    fn read_xattr(&self, node: NodeId, name: &str, max: usize) -> Result<Option<Vec<u8>>>;
}
```

Additions to `Metadata`: `finder_info: Option<[u8; 32]>` (the raw 32 bytes: `FInfo`+`FXInfo` for a file, `DInfo`+`DXInfo` for a folder; `None` when all zero or the format has none) and `VolumeInfo::local_times: bool` (set by classic HFS in M6; declared now so the date formatter is written once: when set, format the timestamp's UTC fields without converting to the local zone).

Implementations (S): `HfsPlus` moves `open_data` behind `open` (keep `open_data` as a thin inherent wrapper until its callers are gone, then delete it), maps `Stream::Resource` to the resource fork with the rules above, turns `xattrs` into `Vec<XattrInfo>`, and implements `read_xattr` over `Attributes::get` (refusing `com.apple.decmpfs` on a compressed file with `Ok(None)`). `NativeFs::open` is a `FileSource` over the interned path; its xattr methods keep the defaults. `atlas_check --read-data` switches to the trait.

## 3. Preview pipeline (`crates/app/src/preview/`) — lead writes `mod.rs`, S the providers

**Done (Fable, 2026-11-12): `preview/mod.rs` is written and tested, and it is the reference.** `preview/image.rs` and `preview/text.rs` exist as providers that accept nothing (task 5a fills them); `preview/structure.rs` is an empty stub (task 3c). The module carries `#![allow(dead_code)]` until `QuickLook` calls it; task 5a removes that line.

What was verified in the GPUI 0.2.2 source, and what it changed from the first sketch:

- `Image::from_bytes` does not decode. `img(ImageSource::Image(..))` decodes lazily on first paint, on the background executor, through `Image::to_image_data` (`platform.rs:1743`), with **no size limit**, decoding **every frame** of a GIF, and with nowhere to report an error. So the pipeline decodes itself, on our background thread, after its own checks, and hands the panel an `Arc<RenderImage>` for `img(ImageSource::Render(..))`.
- `to_image_data` wants the app's SVG renderer whatever the format, and `SvgRenderer` is **not exported** by GPUI 0.2.2 (`gpui.rs:98` is a private `use`). The value can be held and passed, the type cannot be named. Hence `preview::decoder(cx)`, which returns a closure; make it on the UI thread and move it into the task.
- `ImageFormat` has `Png, Jpeg, Webp, Gif, Svg, Bmp, Tiff`. **SVG is never previewed**: it can name other files and fonts on the host. `check_image` refuses it even if a provider returns it.
- GPUI keeps a GPU texture for every painted `RenderImage` until told otherwise. **Whoever evicts a picture from the `QuickLook` cache calls `cx.drop_image(image, Some(window))`** (`app.rs:2071`), or textures leak for the life of the window.
- A panic in the `image` crate's decoders is caught (`catch_unwind`) and becomes an error.

```rust
// preview/mod.rs, as built (doc comments there are the contract)
pub const SNIFF_BYTES: usize = 4096;
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_LINE_BYTES: usize = 4096;              // a longer line continues on the next row
pub const MAX_IMAGE_FILE: u64 = 64 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 64_000_000;        // one frame
pub const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024; // 4 bytes a pixel, every frame

pub struct Subject {
    pub fs: Arc<dyn FileSystem>, pub node: NodeId, pub name: SharedString, pub stream: Stream,
    pub finder_codes: Option<FinderCodes>,
    pub format: Option<&'static str>,                // VolumeInfo::format, for the text fallback
}
pub struct Sniff { pub head: Vec<u8>, pub len: u64 } // head: up to SNIFF_BYTES; len: the stream's

pub struct TextPreview { /* text: Arc<str>, rows: Arc<[Range<u32>]> */ pub encoding: &'static str, pub truncated: bool }
impl TextPreview {
    pub fn new(text: String, encoding: &'static str, truncated: bool) -> Self;  // text has LF endings only
    pub fn row_count(&self) -> usize;
    pub fn row(&self, index: usize) -> Option<&str>;
    pub fn text(&self) -> &str;
}

/// What a provider returns for a picture: still encoded, with its declared size.
pub struct EncodedImage { pub format: ImageFormat, pub bytes: Vec<u8>, pub width: u32, pub height: u32, pub frames: u32 }
pub struct ImagePreview { pub image: Arc<RenderImage>, pub width: u32, pub height: u32, pub frames: usize }

pub enum Content { Text(TextPreview), Image(EncodedImage) }          // provider output
pub enum Preview { Text(TextPreview), Image(ImagePreview), None }     // what the panel shows
impl Preview { pub fn cost(&self) -> u64 }                            // bytes, for the cache budget

pub trait PreviewProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn accepts(&self, subject: &Subject, sniff: &Sniff) -> bool;      // cheap, pure, reads nothing
    /// `Ok(None)` when cancelled. `Err` text is shown to the user.
    fn load(&self, subject: &Subject, sniff: &Sniff, source: &dyn BlockSource, cancel: &AtomicBool)
        -> anyhow::Result<Option<Content>>;
}

pub fn providers() -> &'static [&'static dyn PreviewProvider];        // image, then text
pub type Decoder = dyn Fn(gpui::Image) -> anyhow::Result<Arc<RenderImage>> + Send + Sync;
pub fn decoder(cx: &App) -> Arc<Decoder>;                             // UI thread
/// Blocking. `Ok(None)` when cancelled before the preview was complete.
pub fn load(subject: &Subject, decoder: &Decoder, cancel: &AtomicBool) -> anyhow::Result<Option<Preview>>;
/// For providers: the first `limit` bytes in 1 MiB `read_bulk` pieces, `cancel` checked between.
pub fn read_prefix(source: &dyn BlockSource, limit: usize, cancel: &AtomicBool) -> anyhow::Result<Option<Vec<u8>>>;
```

`load`: a stream other than `Data` → `Preview::None` without opening anything; `fs.open` → empty stream → empty `Preview::Text`; read `min(len, SNIFF_BYTES)` → the first provider whose `accepts` is true → its `load` → for a picture, `check_image` (not SVG, not empty, `width × height ≤ MAX_IMAGE_PIXELS`, `width × height × 4 × frames ≤ MAX_DECODED_BYTES`), then the decoder. The panel turns an `Err` into its message: "This file could not be read (…)." for an engine error, the provider's own sentence otherwise.

**Image provider (`preview/image.rs`), S.** Accepts by magic in `sniff.head`, never by extension, and only when `sniff.len ≤ MAX_IMAGE_FILE`: PNG `89 50 4E 47 0D 0A 1A 0A`; JPEG `FF D8 FF`; GIF `GIF87a`/`GIF89a`; BMP `BM` with a header size at byte 14 of 12, 40, 52, 56, 108 or 124; TIFF `II*\0` or `MM\0*`; WebP `RIFF....WEBP`. `load` reads the whole stream with `read_prefix` and returns `Content::Image` with the format and **the size the file's own header declares**, which is the provider's real work. Every read is `bytes.get(..)`; a header that cannot be parsed is an `Err` ("This picture's header is damaged."), never a guess:

| Format | Width and height | Frames |
|---|---|---|
| PNG | big-endian u32 at 16 and 20 (the IHDR chunk must be first: length 13 at 8, `IHDR` at 12) | 1 (APNG is shown as its first frame by the decoder) |
| JPEG | walk the segments from byte 2: each is `FF`, a marker byte, then a big-endian u16 length that includes itself; the first marker in `C0..=CF` other than `C4`, `C8`, `CC` is the frame header: height u16 at +5, width u16 at +7 from the `FF`. At most 4096 segments; stop at `FF DA` | 1 |
| GIF | little-endian u16 at 6 and 8 | walk the blocks after the header and the global colour table (present when byte 10 has bit 7; `3 × 2^((byte 10 & 7) + 1)` bytes): `21` extension (skip a label byte, then sub-blocks: a length byte and that many bytes, until a zero length); `2C` image (skip 9 bytes, a local colour table sized the same way from bit 7 and the low 3 bits of the 9th byte, one LZW byte, then sub-blocks) and count it; `3B` ends. At most 100,000 blocks. Zero images is an error |
| BMP | header size 12: little-endian u16 at 18 and 20; otherwise i32 at 18 and 22, height by absolute value | 1 |
| TIFF | the first directory at the u32 offset in bytes 4–7 (byte order from the magic): a u16 entry count (at most 4096), then 12-byte entries (tag u16, type u16, count u32, value); tag 256 is the width and 257 the height, each as a SHORT (type 3) or LONG (type 4) held in the value field | 1 |
| WebP | at 12: `VP8 ` (lossy): 14-bit width and height at 26 and 28, little-endian, masked with `0x3FFF`; `VP8L`: byte 20 is `2F`, then 14 bits of width − 1 and 14 bits of height − 1 packed little-endian from byte 21; `VP8X`: 24-bit width − 1 at 24 and height − 1 at 27 | 1 (an animated WebP decodes as one frame) |

Verify each offset against its format's specification before coding, and add one fixture test per format plus a truncated one. If a declared size turns out smaller than what the decoder produces, that is a bug in the table here, not a reason to drop the check.

**Text provider (`preview/text.rs`), S.** Accepts when the sniff looks like text: a UTF-8 or UTF-16 byte-order mark; or no NUL byte and fewer than 5% of bytes below 0x20 other than tab, LF, CR, FF; or type code `TEXT`. Reads at most `MAX_TEXT_BYTES` with `read_prefix`; `truncated` when the stream is longer. Decoding, first that applies: BOM → that encoding; valid UTF-8 (allow a cut multi-byte sequence at the truncated end) → "UTF-8"; UTF-16 without BOM is not guessed; `subject.format == Some("HFS")` or a `TEXT` type code → Mac OS Roman through the M6 table (until M6 lands, Windows-1252); otherwise Windows-1252 ("Western"). Line endings: lone CR (classic Mac) and CRLF become LF. Control characters other than tab and LF are replaced by U+FFFD. Then `TextPreview::new`, which splits the rows (and cuts lines longer than `MAX_LINE_BYTES`) on the same background thread; the view draws `row(i)` and never splits anything.

**State (`state/quick_look.rs`).** A `QuickLook` entity: `open: bool`, `tab: Tab { Preview, Bytes, Details }`, `stream: Stream`, `subject: Option<Subject>`, `preview: LoadState<Preview>`, `generation`, `cancel`. It observes `Listing`; when the primary entry changes while open, it cancels, bumps the generation and loads the new subject after `DEBOUNCE = 30 ms` of quiet (holding Down must not start a load per row). Shows "Loading…" only after 150 ms, like listings. A small cache keeps the last `PREVIEW_CACHE = 8` previews by `(SourceId, NodeId, Stream)` with a byte cap of 64 MiB counted by `Preview::cost()`, so stepping back is instant. A single preview larger than the cap is shown and not cached. Evicting a `Preview::Image` (and closing the panel on an uncached one) calls `cx.drop_image(image, Some(window))`. `preview::decoder(cx)` is made once, when the entity is created, and cloned into each load.

## 4. Quick Look panel (`views/preview.rs`) — S, bytes view O

Verify first: `anchored`, `deferred`, focus trapping and `uniform_list` in the 0.2.2 source and examples.

- A floating panel, `surface`, radius 10, centered over the window at 80% of its width and height (min 480 × 360), drawn with `deferred` so it sits above the list. Fade and scale in over 120 ms unless reduced motion is on. The list underneath stays live: arrows move the selection and the panel follows.
- Header: the name, then the tabs (Preview, Bytes, Details), then, on volumes with `resource_forks` and when the file has a non-empty resource fork, the fork switch "Data | Resource". Switching fork reloads with `stream` changed; the Preview tab for a resource fork is always `None` in M5 (a resource map viewer is a later idea), so it opens on Bytes.
- Keys: Space toggles; Escape closes; Ctrl+1/2/3 would clash with view switching, so tabs use Ctrl+Tab and Ctrl+Shift+Tab; Ctrl+G opens "Go to byte" in the Bytes tab. Focus is trapped in the panel for Tab; arrow keys still go to the list unless the Bytes tab has focus.
- **Preview tab.** Text: a `uniform_list` of lines in the interface face at 13 px, no wrapping in M5 (horizontal scroll), with "Showing the first 256 KB" under the header when truncated, and the encoding name in `ink3`. Image: `img(..)` with `ObjectFit::Contain` on the `hover` well, the pixel size in `ink3` under it.
- **Details tab.** Everything `metadata` has, in technical words (this is where "extents" may be said): the details pane's rows plus node id, all three dates with seconds, raw mode in octal, Finder flags decoded, the extended attributes as a name and size table (through `xattrs`), and "On disk: N pieces" with the first eight runs as byte offset and length (through `extents`). Loaded on the background executor under the same generation.
- **Enter on a file** now opens Quick Look (it did nothing in M1). Ctrl+Enter in search results shows the file in its folder.
- The details pane's preview well shows the same `Preview::Image` scaled down, or the first 12 lines of text at 12 px, or the kind icon. It shares `QuickLook`'s cache; it loads only when the pane is visible.

### 4.1 Bytes view (O: virtualized, async, hot)

```
Offset      00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F   Text
0000A3F0    89 50 4E 47 0D 0A 1A 0A  00 00 00 0D 49 48 44 52   .PNG........IHDR
```

- Mono face (Atkinson Hyperlegible Mono; H downloads the files and `OFL.txt` into `assets/fonts` after Julian confirms, as in M1). 16 bytes per row; offsets in hex, 8 digits, 12 when the stream is 4 GiB or more. Text column: printable ASCII as is, everything else `.`. On a volume or file treated as Mac OS Roman, high bytes show their Roman characters.
- **Window.** `WINDOW = 4 MiB` = 262,144 rows. `window_start` is a multiple of `WINDOW / 2`. The list has the window's rows plus, when there is more, a first row "Earlier bytes" and a last row "Later bytes". Scrolling to within 64 rows of an end row, or activating it, moves the window by half its size and adjusts the scroll offset by the same number of rows in the same update, so the bytes under the eye do not move. Ctrl+G sets the window so the target is inside it and scrolls to its row.
- **Pages.** Data is fetched in `PAGE = 64 KiB` pages by `source.read_at` on the background executor. A `HashMap<u64, Arc<[u8]>>` of at most 32 pages, evicted least-recently-used. A row whose page is absent draws its offset and blank cells and requests the page; requests are deduplicated; each carries the panel's generation. One `notify` per arriving page.
- A read error on a page marks that page failed: its rows show "unreadable" in `ink3` and the error text sits in the side column. Other pages still load.
- **Selection.** Click and drag, or Shift+arrows, select a byte range (the accent, since it is a selection). The side column (220 px) shows for the selection: the offset and length in decimal and hex; for 1, 2, 4 and 8 selected bytes the value as unsigned and signed, big- and little-endian; for 4 bytes also as a four-character code and as an HFS date; for 8 bytes as a Unix-nanosecond date when plausible. Ctrl+C copies the selection as hex text (`cx.write_to_clipboard`), capped at 1 MiB of bytes.

### 4.2 Structure explanations (`preview/structure.rs`) — S

Pure, table-driven, no GPUI.

```rust
pub struct Field { pub range: Range<u64>, pub name: &'static str, pub value: String, pub note: &'static str }
pub struct Structure { pub title: &'static str, pub fields: Vec<Field> }
/// Structures recognised at the start of the stream. `head` is its first 4096 bytes.
pub fn describe(head: &[u8], stream: Stream, len: u64) -> Vec<Structure>;
```

Each layout is a `const` table of `(offset, length, name, FieldKind, note)` with `FieldKind` in `{U8, U16Be, U16Le, U32Be, U32Le, U64Be, U64Le, FourCc, Bytes, PascalString, MacDate, Fixed16_16}` and a match function. Every read goes through `head.get(..)`. When the byte selection intersects a field, the side column shows the structure's title, the field's name, value and note in plain words, and the field's whole range gets a `hover`-tone background in the grid so the user sees its extent.

Layouts for M5 (offsets are from the format specifications; verify each against its primary source and add one fixture test per layout):

| Structure | Recognised by | Fields |
|---|---|---|
| Resource fork header | `stream == Resource`, and the four big-endian u32s at 0, 4, 8, 12 (data offset, map offset, data length, map length) are consistent with `len` | those four; note that the map holds the type list |
| AppleSingle / AppleDouble | u32 BE `0x00051600` / `0x00051607` | magic, version, entry count @24 (u16), then per entry: id, offset, length (12 bytes each from 26), id named (1 data fork, 2 resource fork, 3 real name, 9 Finder info) |
| MacBinary | byte 0 == 0, byte 1 in 1..=63, byte 74 == 0, byte 82 == 0, and `len` ≥ 128 | name (Pascal @1), type @65, creator @69, data length @83, resource length @87, dates @91 and @95 |
| PNG | signature | signature, IHDR length, width @16, height @20, bit depth @24, colour type @25 |
| JPEG | `FF D8` | SOI, first marker and its length, `JFIF`/`Exif` identifier when present |
| GIF | `GIF8?a` | signature, width @6 LE, height @8 LE, flags @10 |
| TIFF | `II*\0` / `MM\0*` | byte order, magic, first directory offset |
| ZIP local header | `PK\x03\x04` | version, flags, method, CRC-32 @14, sizes @18 and @22, name length @26 |
| PDF | `%PDF-` | version text |
| Binary property list | `bplist00` | magic and version |
| Mach-O | `FEEDFACE`, `FEEDFACF`, `CAFEBABE` and byte-swapped forms | magic (named: 32-bit, 64-bit, universal), CPU type, file type, load command count |
| BinHex 4.0 | text starting `(This file must be converted with BinHex` | the banner |

## 5. Copying out (`extract/`) — O

Engine module, no UI. Files: `extract/mod.rs` (API, the walk), `extract/names.rs` (sanitizing), `extract/appledouble.rs`.

```rust
pub struct Item { pub node: NodeId, pub name: String }          // a selected top-level entry

#[derive(Clone, Copy)]
pub enum HostRules { Windows, Unix, MacOs }
impl HostRules { pub fn current() -> Self }

pub struct CopyOptions {
    pub forks: bool,              // write AppleDouble sidecars
    pub hidden: bool,             // copy entries flagged HIDDEN (default true)
    pub rules: HostRules,
}

pub enum CopyEvent<'a> {
    Started { name: &'a str, size: Option<u64> },
    Bytes { total_bytes: u64 },                    // cumulative; at most once per MiB
    Finished { total_files: u64 },
    Failed { path: &'a str, error: &'a Error },    // one file or folder; the copy goes on
    Skipped { path: &'a str, why: Skip },          // Symlink, Special, ForkTooLarge, TooDeep, Hidden
}

pub struct CopySummary {
    pub files: u64, pub folders: u64, pub bytes: u64,
    pub failed: u64, pub skipped_links: u64, pub skipped_other: u64,
    pub renamed: u64,                              // names changed to be legal or unique
    pub cancelled: bool,
    pub first_failures: Vec<(String, String)>,     // up to 100: volume path, error text
    pub created: Vec<PathBuf>,                     // the top-level paths written
}

pub fn copy_out(
    fs: &dyn FileSystem,
    items: &[Item],
    dest: &Path,                                   // an existing host folder
    options: &CopyOptions,
    cancel: &AtomicBool,
    events: &mut dyn FnMut(CopyEvent<'_>),
) -> Result<CopySummary>;
```

`Err` is only for failures that end the whole job: `dest` is not a writable folder, the host disk is full (`io::ErrorKind::StorageFull`), or `dest` vanished. Everything else is counted and the copy continues.

### 5.1 The walk

- `dest` is canonicalized once (`std::fs::canonicalize`); on Windows that yields a verbatim `\\?\` path, so paths longer than 260 characters work without any manifest setting. All host paths are built by `push` on it.
- An explicit stack of `(NodeId, host dir path, depth)`. `MAX_DEPTH = 128`: deeper folders are skipped with `Skip::TooDeep` (hostile nesting; also keeps host paths sane).
- **Cycle guard:** a `HashSet<NodeId>` of folders on the current path from the top-level item down (push on enter, remove on leave). A folder already on the path is skipped. A folder reached twice by different routes (directory hard links without a cycle) is copied twice: that is what the volume shows.
- Per folder: create it (`create_dir`; for a top-level item, after choosing a free name), then list it fully through `read_dir`, then handle files, then push subfolders. A listing error counts as `Failed` for the folder, and what was listed before the error is still copied.
- Kinds: `File` copied; `Directory` walked; `Symlink` skipped and counted (`skipped_links`); `Other` skipped. Symlinks are not followed even when `LINKS_TO_DIRECTORY`.
- `cancel` is checked before each entry and after each 1 MiB written. On cancel the file being written is removed and the summary returns with `cancelled`.

### 5.2 Writing one file

1. Host name: `sanitize` then `unique` (below).
2. `OpenOptions::new().write(true).create_new(true)`. `AlreadyExists` here means a race with something else writing to `dest`: pick the next unique name, up to 100 tries.
3. `fs.open(node, Stream::Data)`, then loop `read_bulk` into a reused 1 MiB buffer and `write_all`. On a read error (a damaged file): stop, remove the partial host file, count `Failed`. A file's readable prefix is not kept: a silently short copy is worse than a named failure.
4. `File::set_times` with the modified time (and, on Windows and macOS, created through `FileTimesExt::set_created`), from `fs.metadata(node)`, fetched only when `forks` is on or times are wanted; the listing's `modified` is enough otherwise. Timestamps outside what the host accepts are skipped silently.
5. When `options.forks`: §5.4.

Folders get their modified time set after their content is written (post-order: keep `(path, time)` on the stack). On Windows a directory handle needs `OpenOptionsExt::custom_flags(0x0200_0000)` (`FILE_FLAG_BACKUP_SEMANTICS`) with `access_mode(0x0100)` (`FILE_WRITE_ATTRIBUTES`); both are `std`, no FFI. A failure to set a time is ignored.

### 5.3 Names (`extract/names.rs`)

```rust
pub fn sanitize(name: &str, rules: HostRules) -> Cow<'_, str>;
/// Tracks the names used in one host folder and makes each new one unique.
pub struct NameSet { /* folded names */ }
impl NameSet {
    pub fn seeded_from_dir(dir: &Path) -> io::Result<Self>;   // for `dest` itself only
    pub fn unique(&mut self, name: &str, rules: HostRules) -> String;
}
```

`sanitize`, in this order:

| Rule | Windows | Unix | macOS |
|---|---|---|---|
| Characters replaced by `_` | `< > : " / \ | ? *` and U+0000–U+001F | `/` and NUL | NUL; `/` becomes `:` (the Finder convention) |
| Trailing dots and spaces | each trailing run replaced by one `_` | kept | kept |
| Names `.` and `..` | `_` and `__` | same | same |
| Empty name | `_` | same | same |
| Reserved device names | `CON PRN AUX NUL COM0–COM9 LPT0–LPT9 CONIN$ CONOUT$`, compared case-insensitively against the part before the first dot, also with `¹ ² ³` as the digit: prefix `_` | — | — |
| Length | at most 255 UTF-16 units | at most 255 bytes | at most 255 UTF-16 units |

Shortening keeps the extension (the part after the last dot, when it is 1–16 characters) and cuts the stem at a character boundary. `unique`: fold the name (lowercase through `char::to_lowercase`; all three rule sets treat the host as case-insensitive, which is right for Windows and macOS and harmless on Linux); when taken, try `stem (2).ext`, `stem (3).ext`, … re-shortening the stem so the result still fits; record the result. A fresh folder made by the copy starts with an empty `NameSet`; `dest` itself is seeded from what is already there. Any change counts in `renamed`.

Tests: a table of names per rule set, including every reserved name with and without extension, names of 255 and 256 units with multi-byte characters at the cut, `"a."`, `"a "`, `" "`, `"."`, collision chains (`A`, `a`, `a (2)` all present), and a property test that `sanitize` output is unchanged by a second `sanitize`.

### 5.4 AppleDouble (`extract/appledouble.rs`)

A sidecar `._<host name>` next to the file or folder, written when `options.forks` and the node has a non-empty resource fork (and is not compressed) or non-zero `finder_info`.

```
0   4  magic 0x00051607            4   4  version 0x00020000
8   16 filler, zeros               24  2  number of entries (1 or 2)
26  12 entry: id 9 (Finder info), offset, length 32
38  12 entry: id 2 (resource fork), offset, length          (when there is a fork)
then the 32 Finder info bytes, then the fork bytes
```

All big-endian. With both entries the Finder info is at offset 50 and the fork at 82; with Finder info only, it is at 38. The fork is streamed like a data fork. A resource fork longer than `u32::MAX - 82` cannot be described: skip the sidecar with `Skip::ForkTooLarge`. The sidecar's name goes through the same `NameSet` (if `._name` is taken, the sidecar is skipped and counted as failed; a renamed sidecar would not be associated by anything). On a host running macOS the `._` file is merged by the Finder when the folder is copied to a Mac volume; that is the point of the format.

Extended attributes other than the resource fork and Finder info are not written anywhere in M5; the summary does not mention them. Say so in the user-facing text of the checkbox: "Keep resource forks and Finder information".

Tests: byte-exact sidecars for the three shapes; a copy of the `test_volume` tree into a temp dir compared file by file; a volume with a directory-hard-link cycle terminates; a folder nested 200 deep stops at 128; cancel mid-file leaves no partial file; a pre-existing name in `dest` yields " (2)"; a file whose data fork is damaged leaves no host file and is in `first_failures`.

Fuzz: none needed here (no raw bytes parsed), but add `copy_out` into a temp dir to the `hfsplus_mount` target behind a size cap of 1 MiB total, so hostile names reach `sanitize`.

## 6. Copy in the app — S

- `state/transfers.rs`: a `Transfers` entity with a queue of jobs and one running at a time. A job holds the source's `fs`, the items (from the selection: node and name of each selected row; in search results the same), `dest`, options, its `cancel`, and live counters. The job runs `copy_out` on the background executor; events go through a channel; the entity drains it on a 50 ms timer while a job runs and notifies at most once per drain.
- **Starting.** "Copy to this computer" (details pane button, the only filled button in the app; Ctrl+Shift+C) → `cx.prompt_for_paths` with `directories: true, files: false, multiple: false` (verify the `PathPromptOptions` fields in `app.rs:1116`) → enqueue. The last destination is remembered for the session and offered first.
- The fork checkbox lives in a small confirmation strip above the footer the first time in a session that the selection is on a volume with `resource_forks`: "Keep resource forks and Finder information ☐   [Copy]". After that the remembered choice is used silently and can be changed from the footer's copy menu.
- **Progress** in the list footer, replacing the item count while a job runs: "Copying 1,204 items · 3.1 GB · Cancel". For a selection of files only, the total is known from the listing and a thin `ink` bar shows the fraction; for folders the total is unknown and no bar is shown. Never the accent.
- **Done.** "Copied 1,204 items to Downloads. Show" where Show calls `cx.reveal_path` on the first created path. With problems: "Copied 1,198 of 1,204 items. 4 could not be read, 2 links were not copied. Details" opening a floating panel with `first_failures` and the counts. Cancelled: "Copy stopped. 312 items were copied."
- A job outlives navigation; it holds its own `Arc<dyn FileSystem>`. Quitting with a job running asks once: "A copy is still running. Quit anyway?"

## 7. Trait housekeeping

Done on 2026-11-12 with the trait additions: `architecture.md`'s sketch no longer has `ReadAt` or `Stream::Xattr` and lists `open`, `xattrs`, `read_xattr`, `scan`, `identity`, `snapshots`, `open_snapshot`. What is left for the end of M5 (H): change its "(defaults …)" note for `open` and the xattr methods to say HFS+ and the host implement them, and delete the sentences in the "HFS+ and HFSX" section about `open_data` and `xattrs` being inherent "until M5".

## 8. Details-pane debts from M3 — S

- "On disk: N pieces" under the size, from `extents` (already loaded for the strip marks: reuse `state/strip.rs`'s result rather than asking twice).
- Kind text: when the extension gives nothing, use the type code through a small table (`TEXT` plain text, `APPL` application, `PICT` picture, `JPEG`, `GIFf`, `PNGf`, `MooV` movie, `AIFF`, `sfil` sound, `PDF `, `ZIP `, `SIT!` StuffIt archive, `rsrc`, `clpp`/`clpt` clipping, `FFIL`/`tfil` font, `ilht`…); else "Document". Compressed files add ", compressed" in the details subtitle only (already there); the Kind column stays the plain kind.

## 9. Tasks, in order

| # | Owner | Task | Files owned |
|---|---|---|---|
| 1 | Lead | **Done.** §2 trait additions and `Metadata::finder_info`, `VolumeInfo::local_times` | `fs/mod.rs` |
| 2 | H | Stubs: `state/{quick_look,transfers}.rs`, `extract/{names,appledouble}.rs` (the `preview/` files already exist); mono font files after Julian confirms | those, `assets/fonts` |
| 3a | S | `open`/`xattrs`/`read_xattr`/`finder_info` on `HfsPlus` and `NativeFs`; `atlas_check` | `hfsplus/mod.rs`, `fs/native.rs`, example |
| 3b | O, parallel | §5 `extract` | `extract/*` |
| 3c | S, parallel | §4.2 structures | `preview/structure.rs` |
| 4 | Lead | **Done.** `preview/mod.rs` from §3, with tests | `preview/mod.rs` |
| 5a | S | Image and text providers, `QuickLook` entity | `preview/{image,text}.rs`, `state/quick_look.rs` |
| 5b | O, parallel | §4.1 bytes view | `views/bytes.rs` (new), its state |
| 6 | S | §4 panel, tabs, fork switch, Details tab, keys, details-pane well | `views/preview.rs`, `components/details.rs`, `keymap.rs`, `app.rs` |
| 7 | S | §6 transfers and copy UI; §8 | `state/transfers.rs`, `components/{details,statusbar}.rs` |
| 8 | Lead | Timings: Quick Look to first content on a text and an image file (traced span, `--profile profiling`); copy throughput from a UDIF image (MB/s) | roadmap |

What Julian tries: Space on a text file, a JPEG, a file with a resource fork (switch fork, read the header explanation in Bytes), a 2 GB file (scroll across window edges, Ctrl+G); copy a folder with odd names (`CON`, a trailing dot, a `/` in a name) to the desktop with forks on and check the `._` files; cancel a large copy.

## 10. Out of scope

Syntax colouring, PDF and movie previews, PICT and icns decoding, a resource map browser, editing anything, drag and drop out of the window, writing extended attributes to the host, recreating symbolic links, resuming a copy.

## 11. Changes during build

Decisions the lead made while running the tasks. Nothing here changes a public interface in §2 or §3.

- **2026-11-12, task 3a (S): file ownership widened.** `hfsplus/catalog.rs` and `end_to_end.rs` were not in the task's file list. `FolderRecord` and `FileRecord` gained `finder_info: [u8; 32]` (record bytes 48..80, read with `d.get(48..80)` inside the existing closure, so a short record is still `Truncated`); `end_to_end.rs` moved its `open_data` call to the trait. `fuzz/fuzz_targets/hfsplus_mount.rs` also moved to the trait (the lead did this one; the fuzz crate does not build on Windows, so that edit is not compiled here) and now also calls `read_xattr` and `open(Stream::Resource)`.
- **Task 3a: `open_data` and the inherent `xattrs` are deleted**, not kept as wrappers: every caller (tests, `atlas_check`, `end_to_end`, the fuzz target) now uses the trait. `architecture.md`'s sentences about them being inherent "until M5" are still to be removed at the end of M5 (§7).
- **Task 3a: no wildcard arm in `HfsPlus::open`.** `Stream` is `#[non_exhaustive]` only for other crates; inside the engine the match is exhaustive and a `_` arm trips `unreachable_patterns` under `-D warnings`.
- **Task 3a: an unresolved hard link is never "compressed"** for `open` and `read_xattr` (it opens its own forks), as `open_data` behaved before. Its empty resource fork gives `NoSuchStream` like any empty fork.
- **Task 3a: `read_xattr` over an attribute kept in a fork reads at most `max` bytes** before answering `Unsupported` when the value is longer (it uses `Attributes::read_head`). Bounded by `max`; nothing larger is allocated.
- **Task 3a: Finder-info offsets** (userInfo 48..64, finderInfo 64..80, both record kinds) agree with the existing field offsets and TN1150 as the agent remembered it; `filesystems.md` does not mention them. The volume-level tests set only part of the pattern (the `test_volume` builders cannot set more); the full 32-byte pattern is tested at the record parser.
- **2026-11-12, tasks 3b (O) and 3c (S): both agents were cut off by the rate limit without a report; the lead verified the code on disk.** 3b was reviewed by a second `opus-engineer` against §5 (no blocking findings). 3c (`preview/structure.rs`, all 12 layouts, fixture, near-miss and random-input tests) was checked by the lead.
- **Task 3b: folder entries are bounded per job.** §5.1's "a folder reached twice is copied twice" let a hostile volume stack directory hard links into about 2^n routes. A folder is now entered at most `MAX_FOLDER_VISITS = 16` times per job (a job-wide count, so work is at most 16 times the number of distinct folders); further routes are `Failed` ("a folder reached through too many routes"). Ordinary shared folders still copy once per route. No public interface changed.
- **Task 3b: `NameSet::unique_for(name, rules, also)`** keeps the unique name within `also`'s length limit; `copy_out` passes the host's rules. `unique` is unchanged (still a pure function of one rule set). Fixes a too-long " (2)" name when the chosen rules are another platform's than the host's.
- **Task 3b: smaller changes.** `renamed` counts after a successful write, not before. An `Unsupported` answer when opening a resource fork means "no fork", like `NoSuchStream`. The test about pre-1601 times was wrong (`SystemTime` holds them; the host call refuses them and that is ignored) and was corrected.
- **Task 3b: accepted deviations from §5.** A `Bytes` event is also sent at the end of each file (documented on `stream`), not only per MiB. A folder already on the path is counted `Failed` with a loop error, not `Skipped` (there is no `Skip` variant for it and adding one changes the public enum). A compressed file with non-zero Finder info still gets a Finder-info-only sidecar (§5.4's "not compressed" applies to the fork). A `metadata` failure after a successful copy with forks on counts the file in both `files` and `failed`.
- **Task 3b: open.** (a) The `copy_out` addition to the `hfsplus_mount` fuzz target (§5.4) is not done; `fuzz/` was outside 3b's files and cannot be built on Windows. (b) `hfs_directory_link_cycle_terminates` asserts only `failed + skipped_other >= 1`; it can be tightened to the exact tree. (c) `canonicalize` failing on some Windows volumes would make a job `Err`; `std::path::absolute` plus a `\?\` prefix is the fallback if it is ever seen. (d) Cancel is not checked inside one very large folder listing.
- **2026-11-12, §5.4 fuzz (lead): done.** `fuzz/fuzz_targets/hfsplus_mount.rs` now calls `copy_everything` after `exercise`: `copy_out` of the volume root into a per-process scratch folder under `std::env::temp_dir()` with `forks: true`, `hidden: true` and `HostRules::Windows` (the strictest, so every sanitizing branch runs on any host), removed afterwards. `copy_out` has no byte cap, so the target cancels it from the events callback once `Bytes.total_bytes` reaches 1 MiB (the real total can reach about 2 MiB, because progress comes once per MiB) or after 4000 events. `cargo check` in `fuzz/` passes on Windows (it compiles, it does not run); the target has not been run under libFuzzer, which needs Linux or macOS. Open item (a) above is closed.
- **2026-11-12, ordering (lead):** at Julian's instruction task 5a runs before the roadmap's search batch (items 7 to 10). The batch touches `state/listing.rs`, so it must not overlap 5a or 6; it runs after 5a, and before 6 if possible.
- **2026-11-12, task 5a (S): accepted.** `preview/image.rs`, `preview/text.rs` and `state/quick_look.rs` are built; 45 new tests; the lead reran fmt, clippy `-D warnings` and `cargo test --workspace` (341 app, 685 lib passing).
- **Task 5a: `dead_code` allowances stay until task 6.** `preview/mod.rs` keeps its `#![allow(dead_code)]`: removing it gives 77 warnings (74 in `structure.rs`, which nothing calls yet; `Subject.name`, `TextPreview::{row_count,row,text}` and `PreviewProvider::name` in `mod.rs`). `quick_look.rs` has its own `#![allow(dead_code)]` because no window builds the entity yet. **Task 6 removes both** once the panel and the structure explanations have callers.
- **Task 5a: `Subject::finder_codes` and `format` are read on the background executor** (`fs.metadata`, `fs.info().format`) inside the load task, since `Listing` does not carry them and the engine may touch the disk. `QuickLook::subject()` therefore returns them as `None`.
- **Task 5a: eviction and `drop_image`.** `close` and `set_stream` take `&mut Window` and pass it to `drop_image` (GPUI takes the updating window out of its list during the update). The async finish path and the listing observer pass `None`. An image is dropped only when it is neither shown nor cached (`Arc::ptr_eq`). **Task 6 must call `toggle`, `close` and `set_stream` with its window.** A replaced picture that is also being evicted can be passed to `drop_image` twice; harmless, left as is.
- **Task 5a: policy left to the panel.** `set_stream(Resource)` does not switch tabs; the fork resets to Data when the selected file changes. Directories and non-file nodes give `Idle`; files and symlinks load.
- **Task 5a: two decoder-side checks beyond §3's table**, both only rejecting what the decoder would reject: the WebP VP8 start code `9D 01 2A`, and the JPEG walk skips fill `FF` bytes and the no-length markers (TEM, RSTn, SOI). A GIF cut short reports the frames counted so far; zero frames, or no trailer within the 100,000-block cap, is "damaged". §3's table is otherwise correct.
- **Task 5a: not verified in a window.** To look at when task 6 lands: a real picture decoding through `preview::decoder`, the 30 ms debounce while holding Down, "Loading…" after 150 ms, texture release on eviction and close.
- **2026-11-12, roadmap item 7 (engine half, O; reviewed read-only by a second `opus-engineer`, no blocking findings).** Measured on 1,019,897 host entries: the intern table cost 148 bytes per entry (peak 215.6 MiB), over the 100-byte budget, so the decided fix was built: `NativeFs::scan` (`scan_tree`) walks with `read_dir`, reports scan-local ids from the new `FIRST_SCAN_ID = 1 << 63`, and never interns (2.3 bytes per entry, peak 84.8 MiB, build time unchanged at about 11 s). Linux paths (`process_memory`, the non-Unicode name test) are uncompiled. **App half still open and blocking for `SourceKind::Host`**: `activate_result` (`state/listing.rs`), `details.rs`, `quick_look.rs` must resolve a hit through `Index::crumbs` plus `lookup` on the background executor; the "This folder" scope (`state/search.rs`, `find_node`) must walk the origin's crumb names down the index (decided: in the app, no new `Index` interface). Known gap: a host name that is not valid Unicode cannot be reached by `lookup`, and two lossy names can collide; it was reachable through interned ids before.
- **2026-11-12, roadmap items 8, 9 (O) and 10 (S): accepted by the lead** (workspace: fmt, clippy `-D warnings`, 365 app and 698 lib tests passing). 8: per-piece top-4096 with a k-way merge, 51.9 ms to 42.3 ms (query `a`, 3.85M hits, 8 threads); the tail of a result set is unordered, as documented. 9: `Positions` (inverse index, checked by `Arc` pointer identity, so a stale table is never used) is built beside every background sort; `show_results` orders only the sorted head (at most 4096 rows) on the UI thread, then the full order replaces it. Side effect to know: for a column sort of a very large result set, the list shows the relevance head for about 10 ms before the sorted order lands. Left alone: `Selection::extend_to` and `select_all` are still O(results) on the UI thread (selection.rs was not in scope). 10: `elide_path_middle` in `format.rs`, `FolderCache` in `file_row.rs`, width probed from the header's Folder cell in `views/explorer.rs`; `FOLDER_SEPARATOR` duplicates the `/` that `state/listing.rs` uses for results' folder text, keep them in step.
- **2026-11-12, roadmap item 7 app half (S): accepted by the lead** (373 app and 698 lib tests passing, clippy and fmt clean). A hit on a `SourceKind::Host` source is a `Reach::Hit` and is resolved by `reach_path` (one `lookup` per path level, on the background executor, under the listing generation), cached per results set (cap 4096). Used by `activate_result`, `components/details.rs`, `state/quick_look.rs` (`Wanted` carries a `Reach`; the cache key is built from the resolved node in `finish`, `Subject.node` is a stand-in until then) and the "This folder" scope (`Scope::{Node, Names}` in `state/search.rs`, `find_by_names` over `Index::subtree`). A scope whose folder is not in the index gives **zero hits** (as before); a fall-back to the whole index needs a status string and was not done. Gone files: a footer note on activate, "This file no longer exists." in Quick Look. Not tried in a window.
- **2026-11-13, task 6 (S): accepted by the lead** (fmt, clippy `-D warnings`, 398 app and 698 lib tests passing). `views/preview.rs` (`QuickLookPanel`: header with tabs, fork switch and close button, Preview tab with text list and image, Details tab, deferred and anchored frame, 120 ms fade), `keymap.rs` (Space, Escape, Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+G, panel-context arrows), `app.rs` (`Workspace` routes Tab into the panel, Escape in the list closes it first), and the details pane's preview well (`components/details.rs`). Both `#![allow(dead_code)]` of task 5a are gone.
- **Task 6: choices to know.** (a) Reduced motion is read from `DISK_ATLAS_REDUCED_MOTION` because GPUI 0.2.2 does not report the system setting. (b) Page Up/Down and Left/Right scroll the text only while focus is inside the panel (after Tab); with focus in the list they keep their list meaning, as §4 says for arrows. (c) Ctrl+G only switches to the Bytes tab; the "Go to byte" prompt belongs to task 5b. (d) The Bytes tab is a placeholder function, `bytes_tab`, that 5b replaces; `focus_order` is where 5b adds its stop. (e) The widest text row is computed on the UI thread once per generation over at most 256 KB; the Details rows are built the same way.
- **Task 6: not verified in a window.** Look at: Space on a text file and on a picture; the fade; holding Down with the panel open (30 ms debounce, "Loading…" after 150 ms); Tab staying inside the panel and Escape returning to the list; the Resource switch on a file with a fork (opens on Bytes); the details-pane well for a picture and for text; whether the frame over the window lets mouse events reach the list (`deferred` frame has no handlers, so it should).
