# Architecture

This is the starting design. Signatures here are sketches; the main session finalizes each one at the milestone that introduces it and updates this file.

## Layers

```
 physical disk / image file
        |
   io        BlockSource: positional reads over a device, an image, or a slice of either
        |
   image     raw, UDIF .dmg, sparsebundle, sparseimage, DiskCopy 4.2  -> a BlockSource
        |
   partition GPT, MBR, Apple Partition Map                           -> slices
        |
   fs        probe a slice, return a FileSystem (HFS+, HFS, APFS, native, ...)
        |
   search / extract     built only on the FileSystem trait
 ------------------------------------------------------------------ crate boundary
   app       GPUI: state, background tasks, views
```

Each layer knows only the one below it. The app knows only `FileSystem`, the search index, and the device and partition descriptions.

## Engine module map (`crates/lib/src`)

```
error.rs
io/         block.rs      BlockSource, sub-range slices, MemSource
            file.rs       image files via positional reads
            cache.rs      chunk cache, read-ahead, sequential scan path
            device/       enumeration and raw access: windows.rs, linux.rs, macos.rs
image/      raw.rs  udif.rs  sparsebundle.rs  sparseimage.rs  dc42.rs
partition/  gpt.rs  mbr.rs  apm.rs
fs/         mod.rs        FileSystem trait, NodeId, Metadata, DirEntry, probe registry
            native.rs     host folders through std::fs
            hfsplus/      volume_header, btree, catalog, extents, attributes,
                          unicode, compression, journal, links, scan, allocation,
                          bytes (test helpers: test_volume, fuzz_seeds)
            hfs/          mdb, btree, catalog, macroman
            apfs/         container, omap, btree, fstree, crypto (late)
search/     index.rs  build.rs  query.rs  persist.rs  kind.rs  fold.rs  order.rs
            bench.rs      timing tests (cfg(test) only)
extract/    copy files out with forks and metadata (AppleDouble)
sizes/      (R13, R14) index.rs  IndexSizes: folder totals and bytes per kind from the search index
            host.rs       HostSizes: a lazily measured folder tree for the host, with its own workers
            watch.rs      Watcher: host change events as batches of folders (the `notify` crate)
mac/        (R12, R17) Mac conventions that are not a filesystem, each a bounded parser:
            conventions.rs (housekeeping names), appledouble.rs (reader), bplist.rs, xattr.rs,
            bundle.rs, trash.rs, resource_map.rs, alias.rs, ds_store.rs
```

`sizes/` and `mac/` are designed in `design/r10-everyday.md` (approved 2026-11-13) and do not exist until their milestones.

Two crates stay two crates until there is a reason to split (a second consumer, compile times). Module boundaries are kept as strict as crate boundaries would be.

Finalized in M0: every layer directory exists with a `mod.rs`. Leaf files exist as stubs for the layers built in M1 to M3 (`io`, `image`, `partition`, `fs/native.rs`, `fs/hfsplus/*`). The leaves of `fs/hfs`, `fs/apfs`, `search` and `extract` are created at their own milestones, so no empty file sits in the tree for longer than a milestone or two. Platform device modules are private and `cfg`-gated inside `io/device/mod.rs`.

Every Rust file starts with the GPL header as a `//` comment block (the form in `crates/app/src/main.rs`), then a blank line, then its `//!` module documentation. The license text is kept out of `//!` so it does not appear in rustdoc.

## Dependencies, lints and profiles (decided in M0)

**Dependencies.**

- Every version is declared once in `[workspace.dependencies]`; crates opt in with `workspace = true`.
- A dependency is added in the change that first uses it, and removed when its last use goes. `serde`, `rayon` and `criterion` are expected (settings and saved index, search, benchmarks) but are not declared until then.
- The engine depends on `thiserror`, plus `miniz_oxide` (zlib) and `lzfse_rust` (LZFSE, and LZVN wrapped as `bvxn` blocks) for decmpfs. The `fuzz/` crate, outside the workspace, adds `libfuzzer-sys` (approved 2026-11-11). `anyhow`, `tracing-subscriber` and `gpui` belong to the app. No async runtime in either crate.
- Approved by Julian on 2026-11-10 for the disk image readers, declared in M3: `bzip2` 0.6 (default pure-Rust backend `libbz2-rs-sys`), `lzfse_rust`, `quick-xml` 0.41 and `base64` 0.22 (UDIF and sparse bundle property lists, the versions GPUI already uses). `lzma-rs` is a dev-dependency only: it cannot bound memory on hostile xz input, so UDIF LZMA chunks use an in-house decoder and lzma-rs only encodes test streams.
- `gpui` is pinned exactly (`=0.2.2`) because rule 6 depends on reading the pinned source. A bump is a main-session task: read the changelog, fix the call sites, re-check the CI package list.
- Licenses must be compatible with GPL-3.0-or-later (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0, Unicode, CC0 are fine). `Cargo.lock` is committed.
- No minimum supported Rust version is declared yet; CI builds on current stable.

**Lints.** The workspace table denies `unsafe_code`, `unused_must_use` and `clippy::undocumented_unsafe_blocks`, and warns on `clippy::all`, `dbg_macro`, `todo`, `print_stdout` and `print_stderr` (use `tracing`). The clippy gate runs with `-D warnings`, so a warning fails it. An approved `unsafe` site carries `#[allow(unsafe_code)]` on the smallest item that needs it, plus the `// SAFETY:` comment.

The engine adds its own crate-level set in `lib.rs`, which turns rule 2 into something the gate checks: `arithmetic_side_effects`, `indexing_slicing`, `cast_possible_truncation`, `cast_possible_wrap`, `cast_sign_loss`, `unwrap_used`, `expect_used`, `panic`, `unreachable`, `unimplemented`. In practice: `checked_*` or `saturating_*` arithmetic, `.get(..)` instead of `[..]`, `try_from` instead of `as` for narrowing. Unit tests are exempt. An `#[allow]` on one of these needs a comment saying why the value cannot be hostile.

**Profiles.** `dev` optimizes dependencies only (`opt-level = 3` for `"*"`). `release` is `opt-level = 3`, fat LTO, one codegen unit, unwinding kept. `profiling` inherits release with thin LTO and line tables, for traced timings without the fat-LTO link time: `cargo run -p disk-atlas --profile profiling`.

## Core traits (sketch)

```rust
/// Positional, thread-safe reads. No shared cursor.
pub trait BlockSource: Send + Sync {
    fn len(&self) -> u64;
    fn sector_size(&self) -> u32;
    /// Fills `buf` completely or returns an error.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);          // CNID on HFS, inode number on APFS, etc.

pub enum Stream { Data, Resource }   // extended attributes are named values, not streams

pub trait FileSystem: Send + Sync + 'static {
    // Final since M1; the code in fs/mod.rs is the reference.
    fn info(&self) -> VolumeInfo;
    fn root(&self) -> NodeId;
    fn metadata(&self, node: NodeId) -> Result<Metadata>;
    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>>;
    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>>;

    // Final since M3 (defaults return Error::Unsupported).
    fn extents(&self, node: NodeId, stream: Stream) -> Result<Vec<Extent>>;
    fn allocation_map(&self, buckets: usize) -> Result<AllocationMap>;

    // Final since M4 (defaults: a breadth-first walk of read_dir; None).
    /// Every entry on the volume, in whatever order is fastest to read. Feeds the search index.
    fn scan(&self, visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>) -> Result<ScanSummary>;
    fn identity(&self) -> Option<VolumeIdentity>;

    // In the trait since 2026-11-12, ahead of M5 and M7, which implement them
    // (defaults: Unsupported, empty, None, empty, Unsupported).
    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>>;
    fn xattrs(&self, node: NodeId) -> Result<Vec<XattrInfo>>;
    fn read_xattr(&self, node: NodeId, name: &str, max: usize) -> Result<Option<Vec<u8>>>;
    fn snapshots(&self) -> Result<Vec<SnapshotInfo>>;
    fn open_snapshot(&self, id: u64) -> Result<Arc<dyn FileSystem>>;
}
```

Design points behind the sketch:

- `read_dir` is paged so a folder with 300,000 entries paints its first screen without reading the rest.
- Listing decided in M1. `read_dir` returns a `DirStream`, an owned, `Send` object with one method, `next_batch(&mut DirBatch, max)`. A stream instead of a resume token because the host filesystem cannot resume from a token, and because dropping the stream is the whole of cancellation. A stream does not borrow the filesystem, so implementations keep their shared state behind an `Arc` and the stream holds a clone.
- `DirBatch` is a page of entries as one name buffer plus fixed 48-byte records (40 until M3 added `resource_size`); `DirEntry<'_>` is a borrowed view into it. No allocation per entry, and a full batch is shared between threads behind an `Arc` without copying.
- A stream that fails keeps the entries it already appended. That is how a damaged folder still lists what could be read.
- Streams report entries in whatever order is cheapest. Ordering is the app's job (see "Listing pipeline").
- `Metadata` and `VolumeInfo` are `#[non_exhaustive]`. Added in M3: `DirEntry::resource_size` and `Metadata::resource_size` (`None` for folders and formats without forks; `Some(0)` for an empty fork and for a compressed file, whose fork holds the compressed bytes), `EntryFlags::COMPRESSED` (listings and `metadata`; a hard link to a compressed file is compressed), `Metadata::finder_codes` (`FinderCodes`, `None` when both are zero), `Metadata::owner` (`Owner { user, group, mode }`, numbers only, `None` for mode 0), `Metadata::attributes` (count, format-internal ones left out, `None` when unreadable), `VolumeInfo::resource_forks` (the file list shows the Resource fork column only then) and `VolumeInfo::journal` (the format-neutral `JournalState`, moved from `hfsplus::journal`, which re-exports it).
- `info` is the one trait method the UI thread may call: it answers from memory. The `Listing` reads it once per navigation and caches `resource_forks` and `journal` for render.
- `Timestamp` is nanoseconds since the Unix epoch in an `i64`, saturating. Converting to local time and to text is the app's job.
- `NativeFs` interns host paths as `(parent id, name)` to hand out stable `NodeId`s. One `NativeFs` per sidebar root (home, each drive), so a lookup can never leave its root.
- `scan` exists because the fastest way to enumerate a volume is format-specific (a sequential pass over the HFS+ catalog leaves, the APFS fs tree, the NTFS MFT). Walking directories through `read_dir` is the slow fallback.
- `extents` and `allocation_map` are in the trait because the disk strip is a first-class feature, not an HFS+ extra. Decided in M3: both speak in bytes of the source the filesystem was mounted from (the partition), so the app adds the partition's start and nothing else; a wrapper offset is the filesystem's business. `Extent { offset, len }` runs come in file order with disk-adjacent runs merged; `Stream::Data` of a compressed file reports where its compressed bytes are (HFS+: the resource fork; inline compression gives none). `AllocationMap { start, len, density: Vec<u8> }` covers the volume's allocation space in equal slices, 0 empty to 255 full, any used space at least 1, at most `MAX_ALLOCATION_BUCKETS` (4096) buckets. It reads the whole allocation record, so the app computes it once per volume on the background executor and caches it. `Stream` has `Data` and `Resource`; extended attributes are read whole through `xattrs` and `read_xattr`, never as streams.
- Resource forks and extended attributes are streams of a node, never separate nodes.

## Block layer (decided in M2)

The code in `io/block.rs` is the reference for the trait; this section records why.

- **Stack.** Base source (`FileSource`, a device, a decoded image) → one `CachedSource` → a `Slice` per partition → the filesystem. The cache sits directly on the base source, so all partitions of a disk share one set of chunks and a slice adds nothing but an offset.
- **Any byte range.** `read_at` takes any offset and length. A source that needs aligned access (raw devices) aligns internally with a bounce buffer. Through the cache every read is already chunk-aligned, so that path is cold.
- **Out of range** is `Error::OutOfBounds`, checked by `check_range` before any I/O. Short reads are not part of the contract: a read fills the buffer or fails.
- **`read_bulk`** is the sequential path from `performance.md`. Same contract as `read_at`; `CachedSource` sends it straight to the inner source, `Slice` forwards it. Default is `read_at`.
- **Sector size** is the device's logical sector, a power of two from 512 to 65536. Files and images report 512. Partition maps count in it.
- **There is no `ReadAt`.** A file's content, as returned by `FileSystem::open`, is an `Arc<dyn BlockSource>`: the same contract (a length, positional thread-safe reads, a bulk path), already implemented by fork and decmpfs sources. Its `sector_size` is 512 and means nothing.

**Chunk cache (`io/cache.rs`).**

- One `ChunkCache` per app, shared behind an `Arc`, with a byte budget (default 256 MB). `CachedSource::new(inner, cache)` registers the source and gets a `u64` key from a counter that never repeats, so a dropped source's chunks can never be served to a new one; they age out.
- Chunks are 64 KiB, aligned in the base source's offset space. The last chunk of a source is short. The sector size must divide the chunk size, which every legal sector size does.
- 16 shards, each a `Mutex` over a map from `(key, chunk)` to a slot and a CLOCK ring of slots. Chunk bytes are `Arc<[u8]>`: a hit clones the `Arc` under the lock and copies outside it. No lock is held during I/O. A poisoned shard lock is recovered, not propagated.
- Two threads that miss the same chunk may both read it; the second insert wins. Rare, harmless, and cheaper than in-flight tracking. Revisit if a trace shows it.
- A read that misses a run of consecutive chunks fetches the run in one inner read.
- **Read-ahead is synchronous.** The engine has no executor of its own. Each `CachedSource` remembers the chunk after its last miss; a miss exactly there is sequential. Sequential misses widen the fetch (2 chunks, then doubling, up to 16 = 1 MiB), clamped to the end of the source; any other miss resets the window. The extra chunks go into the cache. The widening stops at the first chunk already cached, so the fetch stays one contiguous read and nothing cached is read twice; a hit on the chunk at the mark moves the mark past it, so a scan crossing cached chunks stays sequential.
- Inner errors pass through and nothing is cached for the failed range.
- `stats()` returns hit, miss and inner-read counters for tests and benchmarks.

## Partition maps (decided in M2)

The types in `partition/mod.rs` are the reference.

- Each reader (`gpt::read`, `mbr::read`, `apm::read`) returns `Ok(None)` when its signature is absent, `Err` when the map is unusable, and otherwise a `PartitionMap` whose `damage` lists what it worked around.
- `partition::read` tries GPT (at the source's sector size, then 512, then 4096), then APM, then MBR. A protective MBR (`0xEE`) without a readable GPT is an error, not an MBR with one odd partition. A hybrid MBR on an Intel Mac loses to its GPT.
- An MBR-looking sector that is really a FAT or NTFS boot sector (no partition map, a bare volume) is not an MBR: `mbr::read` returns `None` when any entry is malformed (boot flag other than `0x00` or `0x80`, or a used entry of zero length).
- Partitions are in bytes of the source the map was read from. One that runs past the end is clamped and marked `truncated`; one that starts past the end is dropped and recorded as damage. `Partition::slice` turns one into a `Slice`.
- Type GUIDs, system IDs and type strings map to a `TypeHint`. The hint never decides what is mounted; the probe registry reads the contents.

## Probing (decided in M2)

`fs/probe.rs` names what a source (a whole disk or a partition slice) contains, from its bytes alone.

- One read of the first 64 KiB (less if the source is smaller; ISO 9660's descriptor at byte 32769 is inside it). Every probe is a pure function over that buffer: `fn(&[u8], source_len: u64) -> Option<Content>`. No probe does its own I/O in M2.
- `PROBES` is a static, ordered list; the first match wins. Order: APFS (`NXSB` at 32), HFS+ and HFSX (`H+`/4 and `HX`/5 at 1024), HFS with an embedded HFS+ (`BD` at 1024, `H+` embedded signature at 1024+124), plain HFS, Core Storage, NTFS, exFAT, FAT, ext2/3/4, UDF, ISO 9660. More specific signatures go before ones that can co-exist with them (a hybrid CD carries both ISO 9660 and HFS).
- `Content` is a `#[non_exhaustive]` enum with a `format_name()` for the sidebar ("HFS+", "APFS", "Core Storage", "Unknown"). M3 adds a mount function to each registry entry; the app never matches on a format to decide what to do.
- `probe` returns `Content::Unknown` when nothing matches and `Content::Empty` when the buffer is all zeros. An I/O error is an error.

## Devices and elevation (decided in M2)

- `io::device::list() -> Result<Vec<DeviceInfo>>` and `io::device::open(&Path) -> Result<DeviceSource>`. `DeviceInfo { path, model, size, sector_size, removable }`; `size` is rounded down to whole sectors, and a drive whose model query fails is listed with `model: None`. Reads are split at 16 MiB. `DeviceSource` implements `BlockSource` with aligned reads through a bounce buffer.
- **Windows.** Enumerate `\\.\PhysicalDrive0` to `63`, each opened with access 0 (query only, works without administrator rights) through `std::os::windows::fs::OpenOptionsExt::access_mode(0)`. Size and sector size from `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, model and removable from `IOCTL_STORAGE_QUERY_PROPERTY`. Reads open the drive with `File::open` (read access only) and use `seek_read`. FFI is limited to `DeviceIoControl` through `windows-sys`, in `io/device/windows.rs` only, each call under `#[allow(unsafe_code)]` with a `// SAFETY:` comment.
- **Linux** (after Windows): `/sys/block` for the list, `File::open` on `/dev/<name>` for reads, no FFI. **macOS** (after Linux): `/dev/rdiskN`; needs `ioctl` for the size.
- Opening without rights fails with `Error::AccessDenied`. That is the "needs elevation" state, not a failure.
- **Elevation is the app's job** (it is process management, not disk reading). Windows: "Restart as administrator" relaunches the current executable with `ShellExecuteExW` and the `runas` verb, then quits once the new process has started; a declined UAC prompt leaves the app as it was. Linux and macOS: the state shows the command to run (`sudo`/`pkexec`), no button, until the helper process exists.
- The disk holding the running system is listed like any other; reading it is read-only.

## Disk images (decided in M2)

- Each reader opens a host file and yields an `Arc<dyn BlockSource>` that presents the decoded disk; the cache and partition layers sit above it unchanged.
- `image::open(path) -> Result<Image>` sniffs the format from content, not extension: `koly` trailer (UDIF), sparse bundle folder (`Info.plist` + `bands/`), sparse image header, DiskCopy 4.2 header, otherwise raw.
- UDIF keeps its sorted chunk table in memory and decompresses on demand. It holds a small cache of decoded UDIF chunks of its own (a `mish` chunk is often larger than a 64 KiB cache chunk, and decoding it twice for two neighbouring cache chunks is the waste to avoid). Encrypted images (`encrcdsa`/`cdsaencr`) are detected and reported as unsupported.

## App: disks and images (decided in M2)

**Model (`state/sources.rs`).** Every selectable sidebar row is a `Source`; disks and images are `Disk` records that group them.

```rust
pub struct Source {
    id, group, label, format,            // as in M1
    fs: Arc<dyn FileSystem>,             // a real filesystem, or `Unavailable` (below)
    kind: SourceKind,                    // Host | Volume { disk, start, len } | WholeDisk { disk }
    availability: Availability,          // Browsable | Unsupported { format } | NeedsElevation | Failed(text)
}
pub struct Disk {
    id: DiskId, group: SourceGroup,      // Disks or DiskImages
    label, path: PathBuf, size: u64, removable: bool,
    scheme: Option<Scheme>,              // None: no map (the whole disk is one volume) or not read
    volumes: Vec<SourceId>,              // Volume sources in map order
    whole: Option<SourceId>,             // the WholeDisk source, when the disk failed as a whole
}
```

- A disk whose map was read gets one `Volume` source per partition whose hint is not `NotData`, labelled with the partition name or "Partition N", `format` = `Content::format_name()`. A disk with no map is probed whole and gets one `Volume`. A disk that could not be opened or read gets one `WholeDisk` source carrying the failure as its `Availability`: `NeedsElevation` for `Error::AccessDenied`, `Failed(text)` otherwise.
- Until M3 nothing foreign mounts, so every volume is `Unsupported { format }`. Host roots are `Host` + `Browsable` as in M1.
- **`Unavailable`** is an app-side `FileSystem` used as the `fs` of every non-browsable source, so `Location` keeps one shape. Its `info` uses `VolumeInfo::new`, `root` is `NodeId(0)`, and every other method returns `Error::Unsupported`. It is never listed: see below.
- **`Listing` reads availability, not errors.** `Listing` holds the `Sources` handle. On navigation it looks up the location's source: when it is not `Browsable` it sets `LoadState::Unavailable(Availability)` and starts no load. The explorer renders the designed states from that: "Disk Atlas cannot read this partition yet (HFS+).", "Reading physical disks needs administrator rights." with "Restart as administrator" when `elevation::can_restart_elevated()` and `elevation::manual_hint()` otherwise, or the failure text.
- **One `ChunkCache`** (`Arc`, `DEFAULT_BUDGET`) lives in `Sources`; every device and image is wrapped once in a `CachedSource` over it, and partitions are `Slice`s of that.
- **Startup** adds a third background task: `device::list()`, then for each device open → cache → `partition::read` → probe, all on the background executor, one `add_disk` per device on arrival.
- **Open disk image** (sidebar button, Ctrl+O, `keymap` action `OpenImage`): the system file prompt, then `image::open` → the same pipeline → a disk in Disk images. An image format without a reader yet becomes a `WholeDisk` source with `Failed("Disk Atlas cannot read <format> files yet.")`. Opening the same path twice selects the existing entry.
- **Elevation** (`elevation.rs`): Windows relaunches through `ShellExecuteExW` with the `runas` verb from the background executor; on `Restart::Started` the app quits, on `Declined` nothing changes. Elsewhere no button, a hint.
- **The disk strip** reads the current source's `Disk`: size, and each volume's `start`, `len`, label and format. Gaps between partitions are drawn as unallocated. It is hidden for host roots. Shading and marks (M3) come from `state/strip.rs`: a `Strip` entity watches navigation and the listing; the first time a browsable volume is current it reads `allocation_map(512)` on the background executor and caches the result per `SourceId` for the session (failures too, never retried); with exactly one entry selected it reads `extents` for both streams under a request generation (a failing stream adds no marks, the other still shows). Both are turned into fraction runs off the UI thread (16 opacity steps from 6% to 100% of `relief`; marks merged when closer than 1/4096 of the volume), and the bar's `canvas` only maps them to pixels. Marks are the accent, at least 2 px wide, on the current volume only; volumes without a map keep the flat fill.

## HFS+ and HFSX (decided in M3)

The code in `fs/hfsplus/` is the reference once it lands; this records the shape and why.

**Stack.** `partition slice` → `JournalOverlay` (only when a replay patched something) → `locate` (wrapper offset) → `Slice` of the embedded volume → everything else. The overlay sits on the partition, under the wrapper slice, because journal block numbers count `jhdr_size` units from the partition's start, not the embedded volume's. All structures read through the overlay, including the volume header, which is read once from the raw partition to find the journal and again through the overlay. Special files are `ForkSource`s over that stack, and each B-tree reads its nodes from a `ForkSource`. The chunk cache sits under all of it (on the disk), so nothing in HFS+ caches bytes; it caches decoded facts only.

**Modules.**

| File | Holds |
|---|---|
| `bytes.rs` | Checked big- and little-endian field reads, private to `hfsplus`. |
| `volume_header.rs` | `VolumeHeader::parse`, `locate` (bare or wrapped), alternate-header fallback, `hfs_time` (1904 epoch to `Timestamp`, 0 is unset). |
| `extents.rs` | `ExtentDescriptor`, `ForkData`, `ForkType`; `Geometry`; `ForkMap` (sorted runs, logical block to physical block); `ForkSource: BlockSource` over a `ForkMap`; `fork_map` resolving the first eight extents plus the overflow B-tree. |
| `btree.rs` | Format-neutral B-tree: header record, validated `Node` view, `seek` with a caller's key comparator, `LeafCursor` along the leaf chain. Knows nothing of catalog or extents keys, so classic HFS (M6) reuses it. |
| `unicode.rs` | `NameCompare` (TN1150 case-folding `FastUnicodeCompare`, or binary for HFSX `0xBC`), `decompose` (UTF-8 to Apple's decomposed UTF-16, for lookup), `compose` (stored UTF-16 to UTF-8 display form: NFC that never touches or produces excluded code points). Tables generated from TN1150 and Unicode 3.2 data by `tools/hfs_unicode/`, output checked in. |
| `catalog.rs` | Catalog key and records (folder, file, threads), key comparison, `lookup`, thread lookup, the folder listing stream. |
| `links.rs` | File and directory hard links, private metadata folders, symlink targets. |
| `attributes.rs`, `compression.rs` | Attributes B-tree; decmpfs header, sizes and decoding (zlib, LZVN, LZFSE, uncompressed). |
| `journal.rs` | `journal::open` (journal info block, header, the xnu `replay_journal` walk), `JournalState`, `JournalOverlay: BlockSource` (non-overlapping patches, device byte to journal byte, in a `BTreeMap`). |
| `mod.rs` | `HfsPlus`: `mount(source) -> Result<HfsPlus>` and the `FileSystem` impl. |

**Decisions.**

- `BTree` is `Clone` (an `Arc` to its fork plus the copied header fields). A `LeafCursor` owns a clone and one node buffer, so a `DirStream` can own a cursor and move between threads.
- B-tree bounds: node size a power of two from 512 to 32768; node numbers below `total_nodes` and inside the fork; descent depth at most the header depth (hard cap 32) with each child's height one less than its parent's; a leaf walk visits at most `total_nodes` nodes. A node that fails validation fails the operation that read it, not the volume.
- The extents file's own fork never uses overflow records (TN1150), so it is mapped from the header alone. Every other fork, the catalog's included, is mapped through the overflow tree.
- An extent that reaches past the volume's `total_blocks` fails `fork_map`. A logical size that runs past the extents mapped is accepted; reads into the uncovered tail fail as `Corrupt`, so the readable part of a damaged file stays readable.
- **Names.** Listings report composed UTF-8; lone surrogates become U+FFFD. The display form is NFC except that Apple's excluded ranges (U+2000–U+2FFF, U+F900–U+FAFF, all non-BMP) are never changed or composed into, so `decompose(display) == stored` holds for every stored name (tested exhaustively): what the user sees can always be looked up. `lookup` decomposes the given name the way HFS+ stores it and descends the catalog with the volume's comparison. A name that cannot exist on HFS+ (over 255 UTF-16 units) is `Ok(None)`, not an error. A stored `/` is shown as `/`; making it safe on the host is copy-out's job (M5).
- **Catalog.** `Catalog::list(id)` costs one descent: it seeks to `(id, "")`, whose first record is the folder's own thread, so the same step tells a folder (folder thread) from a file (`NotADirectory`) and a missing node (`NotFound`), and the walk continues from there until the parent ID changes. `find` steps over thread records that compare Equal (under case folding a name of only ignorable code points equals the empty thread name). `record(id)` goes thread then key and checks the ID and kind agree. `path(id)` follows threads to the root, bounded by `min(leaf_records, 4096)`, and returns `(id, display name)` pairs, the breadcrumb's shape. Listing kind comes from the BSD mode (`0` counts as a regular file, as Mac OS 9 wrote it); `HIDDEN` is Finder `kIsInvisible` or a leading dot. Record-level errors carry `Location::Record` from `LeafCursor::position()`.
- **Hard links** are resolved while listing: the entry keeps the link's name and hidden flag and takes the target's node id, kind, size and dates. A link that cannot be resolved (no private folder, no target, wrong kind, a link to a link, or a read error while resolving) is listed as a file with no size; `lookup` and `metadata` follow the same rule so they agree with the listing. `LinkResolver` finds the two private folders once at mount (`Catalog::find` on their raw UTF-16 names) and costs one catalog descent per link entry, uncached. The private folders are left out of the root listing and of root `lookup`. Directory hard links can form cycles: anything that walks recursively (the search index) must guard against it.
- **Mount.** `hfsplus::mount(source)`: `locate` → the source or a `Slice` → `read_header` (raw) → `journal::open` on the partition → `Slice` again over its device → `read_header` (replayed) → extents tree (no overflow) → catalog (through the extents tree) → volume name from the root thread → `LinkResolver`. Any of these failing fails the mount, except the journal: only an I/O error there fails it.
- **Journal.** Replayed in memory when macOS would (journaled bit, `lastMountedVersion` `HFSJ`, journal in this volume, not marked for initialization), never written. `HfsPlus::journal()` reports `Clean`, `Replayed { transactions, stopped_early }` or `NotReplayed { reason }`; a journal that cannot be replayed leaves the volume read as it is on disk. Beyond xnu: every size is positive and inside the ring and the device, the walk covers at most one lap, at most `MAX_BLOCKS` (2^20) live blocks, and block checksums are read in 64 KiB pieces. Format notes and traps in `filesystems.md`, "Journal".
- **Symbolic links** report `NodeKind::Symlink` without `LINKS_TO_DIRECTORY` (the target may be outside the volume); `metadata` reads the target from the data fork, at most 1024 bytes.
- `NodeId` is the CNID. `root()` is 2. The volume name is the name in the root folder's thread record.
- Compressed files (`UF_COMPRESSED` on a regular file) report the decmpfs uncompressed size in listings and `metadata`, at one attributes-tree descent per compressed entry; a header that cannot be read gives `size: None`. Opening file content (`FileSystem::open`) joins the trait in M5 with its caller; until then `HfsPlus::open_data` (a `BlockSource` over the data fork or the decompressed content, hard links followed, `IsADirectory` for folders) and `HfsPlus::xattrs` are inherent. `xattrs` and the `Metadata::attributes` count leave out `com.apple.decmpfs` on a compressed file, as macOS does (Julian, 2026-11-11); decompression reads it directly, and copy-out must not write it.
- **Attributes.** Opened at mount from CNID 8; an empty fork means no tree, a damaged one fails the mount. Keys order by file ID, then name as raw UTF-16 units (case-sensitive on every volume), then start block (xnu `hfs_attrkeycompare`). Fork values follow `0x30` continuation records keyed by blocks mapped so far, bounded by leaf records and `total_blocks`. Values are read whole up to a caller's cap.
- **decmpfs.** Types 1, 3, 7, 9, 11 (in the attribute, decoded once, at most 16 MiB) and 4, 8, 10, 12 (resource fork, 64 KiB chunks decoded on demand, last chunk cached). The whole chunk table is validated at open: count from the size, chunks in order, after the table, inside the fork, 1 byte to 128 KiB. Every chunk decodes to exactly its expected length into a fixed buffer, so a decompression bomb fails without allocating. Raw prefixes: zlib low nibble `0xF`, LZVN `0x06`, LZFSE `0xFF`, types 9/10 `0xCC`. Layouts from apfs-fuse `Decmpfs.cpp` and libfshfs docs; LZVN chunks are assumed to end with the end-of-stream opcode, as Apple's encoder writes. LZBITMAP (13, 14) is `Unsupported`. Not yet checked against a real volume.
- Mounting goes through the probe module: `probe::mount(source, content) -> Result<Option<Mount>>` matches on every `Content` variant (a new variant forces a decision); `Ok(None)` means no reader yet. `Mount` is `Volume(fs)` for every format so far and `Container(volumes)` for APFS in M7 (`design/m7-apfs.md` §7). The app mounts each volume on the background executor right after probing (`prepare` → `mount_volume` in `state/sources.rs`): mounted is `Browsable` and takes the volume's own name as its label, `Ok(None)` is `Unsupported { format }`, an error is `Failed("This volume could not be read (…).")`.

## Threading

- The engine is blocking and re-entrant. Every public type is `Send + Sync` and methods take `&self`. Caches use interior mutability with sharded locks.
- The engine does not use Tokio or any async runtime. The app calls it from GPUI's background executor. Search and index builds use a Rayon pool. (The README lists Tokio "where needed"; so far nothing needs it.)
- The app tags every background request with a generation number. Results for a folder or a query the user has left are dropped on arrival.
- **One exception to "the engine owns no threads", from R14** (`design/r10-everyday.md` §3.2, §3.3; Fable's decision, 2026-11-13): `sizes::HostSizes` owns one to four named worker threads and `sizes::Watcher` one flush thread. They exist because measuring and watching outlive any one call. Both are started on first use, joined on `shutdown` and on drop, and reach the app only through a callback that the app turns into a channel message. Nothing else in the engine starts a thread.

## Errors

- Engine: `thiserror` enums. An error names the structure being read and its location, for example "catalog B-tree node 4127: record offset 9000 past node size 8192". Corruption in one node fails that operation, not the volume.
- App: `anyhow` at the edges, turned into user-facing states: unsupported format, damaged structure, encrypted, needs elevation.
- A damaged directory still lists what could be read, with a visible note that the listing is incomplete.

## Reading physical disks

- Windows: `\\.\PhysicalDriveN`, administrator rights, reads aligned to the sector size.
- Linux: `/dev/sdX` or `/dev/nvme*`, root or membership of the `disk` group.
- macOS: `/dev/rdiskN`, root.

First version: the app runs unprivileged. Disk images always work. Selecting a physical disk without rights shows a "needs elevation" state with a button that relaunches the app elevated. A small read-only helper process is the later, better answer and is a main-session design task.

## App structure (`crates/app/src`)

```
main.rs          startup: logging, assets, theme and fonts, keymap, the window
app.rs           Workspace, the root view: creates the state entities and the views, lays them out
assets.rs        embedded icons (AssetSource over include_bytes!)
format.rs        sizes, dates and kinds as text
keymap.rs        actions and key bindings
theme/           mod.rs (Theme global, metrics), colors.rs (day and night), fonts.rs
state/           sources.rs      Sources: what the sidebar lists
                 navigation.rs   Navigation: Location, history
                 listing.rs      Listing: the rows of the current folder, sort, selection
                 loader.rs       background half of the listing pipeline   (no GPUI types)
                 order.rs        sort keys and comparators                  (no GPUI types)
                 selection.rs    selection set, cursor, anchor              (no GPUI types)
components/      titlebar (the top bar), breadcrumb, sidebar, disk_strip, file_row,
                 details, toolbar (search filters, M4), statusbar (list footer)
views/           explorer (the file list), search, preview (Quick Look)
preview/         mod.rs (provider trait, load pipeline), image.rs, text.rs, structure.rs  (no GPUI types except the final image handle)
```

Added by the R milestones (`design/r10-everyday.md`; each file appears with the milestone in brackets):

```
state/           settings.rs [R10a]   Settings global and the SETTINGS table
                 menus.rs [R11]       what each context menu holds (pure)
                 sizes.rs [R13]       Sizes: the size table of each source
                 visibility.rs [R16]  the one rule for hidden and housekeeping items (pure)
                 facts.rs [R18]       fact loaders shared by details, Quick Look, Properties (no GPUI types)
                 properties.rs [R18]  Properties and the registry of its windows
components/      menu.rs [R11], storage_bar.rs [R15] (replaces the drawing in disk_strip.rs)
views/           aux_window.rs [R5]   the frame Quick Look, Properties and Settings share
                 settings.rs [R10b], properties.rs [R18]
```

State lives in a few GPUI entities. Views read from them and render; they hold no data of their own beyond scroll position and focus.

## App state (decided in M1)

**Entities.** `Workspace` owns all of them and hands each view the handles it needs.

| Entity | Holds | Changed by |
|---|---|---|
| `Sources` | The sidebar's roots: id, group, label, `Arc<dyn FileSystem>`. M1: home and host drives, found on a background thread at startup. | Itself; M2 adds disks and images. |
| `Navigation` | The current `Location` and the back/forward history. Emits `NavigationEvent::Changed`. | Sidebar, breadcrumb, list, key bindings, all through its methods. |
| `Listing` | The current folder's rows, their order, the sort, the selection and cursor, the load state. | Itself, in response to `Navigation`; the list view through its methods. |

Data flows one way: input calls a method on `Navigation` or `Listing`; `Listing` subscribes to `Navigation`; views observe entities and re-render. No view writes another view's state, and nothing reaches around `Navigation` to change the folder.

**Location.** `Location { source: SourceId, fs: Arc<dyn FileSystem>, crumbs: Arc<[Crumb]> }` with `Crumb { node, name }`; `crumbs` is never empty and its first element is the source's root. The app remembers the path it walked, so the breadcrumb needs no "parent of" call on the filesystem. History entries carry the name the cursor was on, so Back puts it there again, and Up lands on the folder just left.

**Selection lives inside `Listing`**, not in its own entity. It is indexed by row arrival number, so it must be cleared in the same update that replaces the rows; two entities could be seen out of step.

### Listing pipeline

```
UI thread                                   background executor
---------                                   -------------------
Listing::load(location)
  generation += 1, rows cleared
  spawn task ──────────────────────────────▶ Loader::step()
                                               read one page into a DirBatch (256 entries first,
                                               then 4x up to 16,384)
                                               sort the page, merge it into the running order
  apply(generation, step) ◀──────────────────  Step { page: Arc<DirBatch>, order: Arc<[u32]>, outcome }
    drop if generation is stale
    push the Arc, swap the order, notify once
  ──────────────────────────────────────────▶ Loader::step() ... until Done or Failed
```

- **Rows** are `Rows { pages: Vec<Arc<DirBatch>> }`. An entry is named by its arrival index (`u32`); that index never changes while the folder is shown, so the selection survives re-ordering. `Rows` is cheap to clone, which is how the loader and a later re-sort share the data with the UI without copying it.
- **Order** is a permutation of arrival indices, built only on the background executor and handed over as an `Arc<[u32]>`. The UI thread never compares two names. Applying a step is a push, a pointer swap and a `notify`: one notify per page, and GPUI draws at most once per frame.
- **Sort**: folders first, then the chosen column (name, modified, size, kind), ties by name. Names compare case-insensitively with digit runs as numbers ("file2" before "file10"). A page that arrives already in order after the last row is appended without a merge, which is the whole cost for HFS+ catalogs and NTFS folders.
- **Changing the sort** of a loaded folder sorts a clone of `Rows` in the background under a new generation; the old order stays on screen until the new one lands. Changing it mid-load restarts the load.
- **Cancellation** is dropping the task: the `DirStream` goes with it. The generation check is the guarantee for a step already in flight.
- **Failure**: the rows read before an error stay, with a note ("Part of this folder could not be read"). A folder that cannot be opened at all is a `Failed` state with a sentence saying why.
- **Loading text** appears only if no row has arrived after 150 ms.
- **Not in M1**: keeping listed folders for instant Back (comes with image-backed volumes, which cannot change under us), watching the host folder for changes, free space in the footer (std cannot ask; needs platform calls).

Budgets this must meet (`performance.md`): first rows of a folder under 50 ms; 100,000 entries listed and ordered under 500 ms; cursor movement, selection and type-to-select never do work proportional to the folder on the UI thread beyond a scan of the `u32` order.
