# M6. Classic HFS

Goal: floppies, CD-ROMs and old hard disk images in classic HFS mount and browse like HFS+ volumes, with names in the right script, and DiskCopy 4.2 images open.

Primary source: *Inside Macintosh: Files*, chapter 2 ("File Manager", data organization on volumes). Cross-checks: the libyal `libfshfs` format document, hfsutils' documentation.

**Verification (Fable, 2026-11-12).** Every offset and size in §2.1 (master directory block, 162 bytes), §2.2 (extent key, 8 bytes with its length byte), §2.3 (catalog key, file record 102 bytes, folder record 70, thread record 46, `FInfo`, `FXInfo`, `DInfo`, `DXInfo`) was checked field by field against the packed struct definitions in the Linux kernel's `include/linux/hfs_common.h`, by computing each field's offset mechanically. **All agree; no table changed.** That is an independent second source, not Inside Macintosh itself: the kernel's layout has read real HFS volumes for twenty-five years, which is the stronger test of the two. Still open, for the task that writes each parser: the order of the extents key's first two fields in comparisons (file id before fork type, as §2.2 says and as the kernel's `hfs_ext_keycmp` does), the key padding rule in `btree.rs` with big-keys clear (§2.3 names the unit test), and the DiskCopy 4.2 header (§5), which no kernel reads; check that one against Apple's technical note or a real image. Facts were read from the kernel header; no code or table was copied from it.

## 1. Decisions

1. **Reuse, do not fork.** `hfsplus::btree` already handles 8-bit key lengths (it switches on the header's big-keys attribute, which classic HFS leaves clear) and any node size. `ForkMap` and `ForkSource` already map block runs to bytes. Classic HFS adds key and record parsers and a mount, nothing below that.
2. **Allocation blocks through a slice.** HFS numbers allocation blocks from a start sector, not from the volume start. Mount builds `Slice(source, alloc_start_bytes, alloc_blocks × block_size)` and calls it the *block area*; inside it, allocation block `n` is at byte `n × block_size`, exactly HFS+'s geometry, so `ForkSource` is reused unchanged. `extents()` adds `alloc_start_bytes` back.
3. **No Apple ordering table.** HFS orders catalog names with a Mac OS Roman relational-string table that exists only in APSL and GPL sources. A read-only explorer needs it for exactly one thing, descending the tree to a *named* key. Design around it:
   - Listing seeks to `(folder id, "")`. The empty name sorts before every name under any table, so the descent compares parent ids and, on equal ids, treats the target as smaller. No table.
   - `lookup(dir, name)` lists the folder and compares names. Classic HFS folders are small (the catalog node is 512 bytes; a 2 GB volume holds at most 65,535 files in total), and `lookup` is only used for typed paths.
   - Thread lookups use `(id, "")`: same as listing.
   (README decision 7. If Julian prefers a real keyed lookup later, it is one function to replace.)
4. **Name equality for `lookup`:** decode both to Unicode, compare with `char::to_lowercase` per character. Case-insensitive, accent-sensitive, which matches HFS's equality for Roman (two names differing only by accents are different files).
5. **Dates are local time with no zone recorded.** They are stored as if UTC and `VolumeInfo::local_times = true` tells the app to print the fields as they are (declared in M5).
6. **Encodings:** single-byte Mac scripts from Unicode.org's published Apple mapping tables. Multi-byte scripts are deferred (README decision 8).
7. **MFS** (400 K floppies, signature `D2 D7` at byte 1024) is detected and reported as unsupported; add `Content::Mfs` with `format_name` "MFS". It is a flat directory format and could be an afternoon's work later.

## 2. On-disk layout

Everything big-endian.

### 2.1 Master directory block, 162 bytes at byte 1024

| Offset | Size | Field | Use |
|---|---|---|---|
| 0 | 2 | `drSigWord` | `0x4244` ("BD") |
| 2 | 4 | `drCrDate` | volume created (local) |
| 6 | 4 | `drLsMod` | last modified |
| 10 | 2 | `drAtrb` | bit 8: unmounted cleanly; bit 15: software lock |
| 12 | 2 | `drNmFls` | files in the root |
| 14 | 2 | `drVBMSt` | first sector of the volume bitmap (512-byte sectors) |
| 16 | 2 | `drAllocPtr` | — |
| 18 | 2 | `drNmAlBlks` | allocation blocks on the volume |
| 20 | 4 | `drAlBlkSiz` | allocation block size, a multiple of 512 |
| 24 | 4 | `drClpSiz` | — |
| 28 | 2 | `drAlBlSt` | first sector of allocation block 0 |
| 30 | 4 | `drNxtCNID` | next unused CNID |
| 34 | 2 | `drFreeBks` | free allocation blocks |
| 36 | 28 | `drVN` | volume name: length byte (1–27) then bytes |
| 64 | 4 | `drVolBkUp` | — |
| 68 | 2 | `drVSeqNum` | — |
| 70 | 4 | `drWrCnt` | write count |
| 74 | 4 | `drXTClpSiz` | — |
| 78 | 4 | `drCTClpSiz` | — |
| 82 | 2 | `drNmRtDirs` | folders in the root |
| 84 | 4 | `drFilCnt` | files on the volume |
| 88 | 4 | `drDirCnt` | folders on the volume |
| 92 | 32 | `drFndrInfo` | — |
| 124 | 2 | `drEmbedSigWord` | `0x482B` when wrapping HFS+ (handled by `hfsplus::locate`, not here) |
| 126 | 4 | `drEmbedExtent` | start block u16, block count u16 |
| 130 | 4 | `drXTFlSize` | extents overflow file size in bytes |
| 134 | 12 | `drXTExtRec` | three extents: (start block u16, block count u16) × 3 |
| 146 | 4 | `drCTFlSize` | catalog file size in bytes |
| 150 | 12 | `drCTExtRec` | three extents |

A copy of the MDB sits in the second-to-last 512-byte sector of the volume; use it when the primary's signature or geometry is invalid (same fallback idea as the HFS+ alternate header).

**Validation at mount** (each failure is `Corrupt` with structure "HFS master directory block" and `Location::Byte`):

- `drAlBlkSiz` non-zero, a multiple of 512, at most 2^31.
- `drNmAlBlks` non-zero.
- `alloc_start = drAlBlSt × 512`; `alloc_len = drNmAlBlks × drAlBlkSiz`; both with checked arithmetic. If `alloc_start + alloc_len` exceeds the source, clamp `alloc_len` to what exists (a truncated image still lists; reads of missing blocks fail on their own) but fail when `alloc_start >= source.len()`.
- `drVN` length 1–27.
- The catalog's first extent is non-empty.

### 2.2 Extents

An extent is `(start u16, count u16)`; a record is three. A fork has its first record in the catalog file record (or the MDB for the two special files); further records are in the extents overflow tree.

Extents overflow key (8 bytes): `keyLength` u8 = 7, `forkType` u8 (`0x00` data, `0xFF` resource), `fileID` u32, `startBlock` u16 (the fork-relative block the record starts at). Data: one extent record (12 bytes). Order: file id, then fork type, then start block. **Verify the order of the first two against Inside Macintosh**; HFS+ orders by file id then fork type, and classic HFS is believed to be the same.

The extents overflow file itself has only its three MDB extents; it cannot overflow. The catalog file can, with file id 4.

`hfs::fork_map(geometry, file_id, fork_type, logical_size, first: [Extent; 3], overflow: Option<&BTree>) -> Result<ForkMap>`: collect runs from `first`; while the blocks mapped so far cover less than `ceil(logical_size / block_size)`, seek the overflow tree for `(file_id, fork_type, blocks_mapped)` and append its record; stop on a missing record (the fork is then shorter than its size says; reads past the mapped part fail, as in HFS+). Bounds: every run inside `drNmAlBlks`; at most `MAX_EXTENT_RECORDS = 65_536 / 3 + 1` records (a fork cannot have more runs than the volume has blocks); a record that adds zero blocks ends the loop.

`ForkMap` has no public constructor today. Add `pub(crate) fn from_runs(logical_size: u64, runs: Vec<ExtentDescriptor>, geometry: Geometry) -> Result<ForkMap>` in `hfsplus/extents.rs` (lead approves the two-line visibility change; O makes it), with the same validation `fork_map` applies to runs. `Geometry { block_size, total_blocks }` is `{ drAlBlkSiz, drNmAlBlks }`.

### 2.3 Catalog

512-byte nodes (take the size from the header record like HFS+; do not hard-code). Catalog file id 4, extents file id 3, root folder id 2, root's parent id 1, first user id 16.

**Key:** `keyLength` u8, reserved u8, `parentID` u32, then the name as a length byte (0–31) and that many bytes. In index nodes the key is padded to its full 37 bytes. After the key, one pad byte when needed so the record data starts on an even offset from the record start. (The shared `Record` view already splits key and data for 8-bit keys with this padding rule for HFS+ small-key trees; **check that the padding rule in `btree.rs` applies when big-keys is clear**, and add a unit test with an odd-length key.)

**Data records.** Byte 0 is the type, byte 1 reserved.

Folder (type 1, 70 bytes):

| Offset | Size | Field |
|---|---|---|
| 2 | 2 | flags |
| 4 | 2 | valence (entries inside) |
| 6 | 4 | folder id |
| 10 | 4 | created |
| 14 | 4 | modified |
| 18 | 4 | backed up |
| 22 | 16 | `DInfo` (Finder flags u16 at +8) |
| 38 | 16 | `DXInfo` |
| 54 | 16 | reserved |

File (type 2, 102 bytes):

| Offset | Size | Field |
|---|---|---|
| 2 | 1 | flags (bit 0: locked) |
| 3 | 1 | file type (unused, 0) |
| 4 | 16 | `FInfo`: type @+0, creator @+4, Finder flags u16 @+8 |
| 20 | 4 | file id |
| 24 | 2 | data fork first block (unused) |
| 26 | 4 | data fork logical size |
| 30 | 4 | data fork physical size |
| 34 | 2 | resource fork first block (unused) |
| 36 | 4 | resource fork logical size |
| 40 | 4 | resource fork physical size |
| 44 | 4 | created |
| 48 | 4 | modified |
| 52 | 4 | backed up |
| 56 | 16 | `FXInfo` (script byte at +8: high bit set means the low 7 bits are a script code) |
| 72 | 2 | clump size |
| 74 | 12 | data fork first extent record |
| 86 | 12 | resource fork first extent record |
| 98 | 4 | reserved |

Thread (types 3 folder, 4 file; 46 bytes): reserved 8 bytes at 2, parent id u32 at 10, name (length byte + up to 31) at 14. Keyed by `(own id, "")`. File threads are optional in HFS (rare); folder threads always exist.

Finder flags: `0x4000` invisible, `0x8000` alias. `EntryFlags::HIDDEN` = invisible flag (classic Mac OS has no dot-file convention; do not hide leading dots).

Dates: unsigned seconds since 1904-01-01, **local**. Reuse `hfs_time`; 0 is unset.

There are no hard links, symbolic links, permissions, extended attributes or compression. Aliases are ordinary files (flag `0x8000`); list them as files. The app already italicises links by flag: add `EntryFlags::ALIAS` only if Julian wants aliases italic too; not in this milestone.

### 2.4 Volume bitmap

Starts at sector `drVBMSt`, one bit per allocation block, most significant bit first, 1 = used, `ceil(drNmAlBlks / 8)` bytes (at most 8 KiB). `allocation_map`: read it whole, reuse the density folding from `hfsplus/allocation.rs` (make its bucket function `pub(crate)` and generic over a bit slice if it is not already). `AllocationMap { start: alloc_start, len: alloc_len, density }`.

## 3. Module layout (`fs/hfs/`)

| File | Holds | Owner |
|---|---|---|
| `mdb.rs` | `Mdb::parse`, validation, alternate fallback, `Geometry`, the block-area slice | S |
| `extents.rs` | extents key and record parse, key comparator, `fork_map` | O |
| `catalog.rs` | key and record parsers, `Catalog { tree, encoding }`: `list(id) -> HfsDir` (a `DirStream`), `record(id)`, `thread(id)`, `find(parent, name)` by folder scan, `volume_name` | O |
| `encoding.rs` (roadmap says `macroman`) | `Encoding` enum, decode and encode | S |
| `mod.rs` | `Hfs`, `mount(source)`, `mount_with(source, Encoding)`, the `FileSystem` impl | O |
| `test_volume.rs` | a builder like `hfsplus/test_volume.rs` | S |

`FileSystem` mapping:

- `info`: name from the root thread (fall back to `drVN`), format "HFS", total = `alloc_len`, free = `drFreeBks × drAlBlkSiz`, `resource_forks: true`, `journal: None`, `local_times: true`.
- `root`: `NodeId(2)`. `NodeId` is the CNID.
- `metadata`: `record(id)` through the thread then the keyed record. Because a keyed descent by name is off the table (decision 3), find the record by walking the parent's listing from `(parent, "")` until the id matches. A file with no thread record cannot be found by id from nothing: such nodes are only reachable through a listing, so `HfsDir` entries must be self-sufficient and the app's `metadata(node)` call after a listing needs help. Solution: `Hfs` keeps a small `Mutex<HashMap<u32, u32>>` from file id to parent id, filled by every listing (cap 1,000,000 entries, then cleared); `metadata` uses the thread when there is one, else that map, else `NotFound`.
- `lookup`: decision 3.
- `read_dir`: seek `(id, "")` with the parent-only comparator; the first record must be the folder's own thread (else `NotADirectory` or `NotFound`, as HFS+); walk leaves until the parent id changes. Same bounds as the HFS+ leaf walk.
- `open`, `extents`: through `fork_map`; `extents` adds `alloc_start`. `Stream::Resource` on an empty fork is `NoSuchStream`.
- `xattrs`: empty. `read_xattr`: `Ok(None)`.
- `scan`: `scan_leaves` over the catalog (M4), visiting folder and file records; no links to resolve. Parent of each record is the key's parent id.
- `identity`: `id` = `drCrDate`, `drNmAlBlks`, `drAlBlkSiz`, then the first 6 bytes of the volume name padded with zeros (classic HFS has no unique identifier; this is the best there is). `change` = `drWrCnt`, `drLsMod`, `drFilCnt`, `drDirCnt`.
- `finder_info`: the 32 bytes `FInfo`+`FXInfo` or `DInfo`+`DXInfo`. `finder_codes` from `FInfo`. `owner: None`.

Registration: `probe::mount` maps `Content::Hfs` to `hfs::mount`. A wrapped HFS+ volume keeps going to `hfsplus` (the probe already tells them apart).

## 4. Encodings (`fs/hfs/encoding.rs`) — S

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum Encoding { #[default] Roman, CentralEuropean, Cyrillic, Greek, Turkish, Croatian, Icelandic, Romanian }

impl Encoding {
    /// Appends the decoded text. Every byte decodes; nothing fails.
    pub fn decode_into(self, bytes: &[u8], out: &mut String);
    /// `None` when a character has no byte in this encoding.
    pub fn encode(self, text: &str) -> Option<Vec<u8>>;
    pub fn name(self) -> &'static str;       // "Mac OS Roman", ...
    pub fn from_script_code(code: u8) -> Option<Self>;   // 0 Roman, 7 Cyrillic, 6 Greek, 29 Central European, ...
}
```

- Bytes 0x00–0x7F are ASCII in every one of these. Each encoding is a `[char; 128]` for 0x80–0xFF.
- Source of the tables: the mapping files Apple published through Unicode.org (`ROMAN.TXT`, `CENTEURO.TXT`, `CYRILLIC.TXT`, `GREEK.TXT`, `TURKISH.TXT`, `CROATIAN.TXT`, `ICELAND.TXT`, `ROMANIAN.TXT` under `Public/MAPPINGS/VENDORS/APPLE/`). They may be used freely under the Unicode licence. Generate the arrays with a small tool `tools/mac_encodings/` (outside the workspace, like `tools/hfs_unicode/`), keep the downloaded `.TXT` files and their SHA-256 hashes beside it, and check in the generated `encoding/tables.rs`.
- Where a mapping file gives a sequence (a base character plus a combining mark, or the Apple logo as a private-use character), use the single precomposed character when one exists; the Apple logo is U+F8FF. Where the file marks a byte as undefined, map it to U+FFFD. The encoder is the inverse of the table (a sorted `[(char, u8); 128]` and binary search); U+FFFD never encodes.
- In Roman, a name byte `0x00` or a `:` cannot occur on disk; if present, decode it as is (the copy-out sanitizer deals with it).
- The mount decodes all names with one `Encoding`. `mount` uses `Roman`. The per-file script byte in `FXInfo` is **not** used to switch per name in M6: mixed-script volumes are rare and a wrong guess is worse than consistent Roman. `mount_with` exists so the M8 settings work can add a per-volume "Text encoding" choice; until then the app always uses Roman.
- Tests: round trip of all 256 bytes for each encoding (decode then encode gives the byte back, except bytes mapped to U+FFFD); spot values from the mapping files (Roman `0x8E` é, `0xA5` •, `0xDB` €, `0xF0` U+F8FF; Cyrillic `0x80` А).

This module also serves M5's text preview ("Mac OS Roman" decoding) and the bytes view's text column; expose it as `disk_atlas_lib::fs::hfs::encoding`.

## 5. DiskCopy 4.2 (`image/dc42.rs`) — S

Header, 84 bytes, big-endian:

| Offset | Size | Field |
|---|---|---|
| 0 | 64 | disk name: length byte (≤ 63) then bytes |
| 64 | 4 | data size in bytes |
| 68 | 4 | tag size in bytes |
| 72 | 4 | data checksum |
| 76 | 4 | tag checksum |
| 80 | 1 | disk format (0 400K, 1 800K, 2 720K, 3 1440K) |
| 81 | 1 | format byte |
| 82 | 2 | `0x0100` |

`image::detect` already recognises it. The reader: validate (`0x0100` at 82; name length ≤ 63; data size non-zero and a multiple of 512; `84 + data size + tag size <= file length` with checked arithmetic), then the image is `Slice(FileSource, 84, data_size)`. Tags are ignored. The checksum (sum of big-endian 16-bit words, rotating the 32-bit sum right by one after each add) is **not** verified at open: a damaged floppy image should still open; `atlas_check` may print whether it matches. Wire into `image::open`. Fuzz target `dc42` with one seed. An 800K image usually holds a bare HFS (or MFS) volume with no partition map; the existing "no map: probe whole" path handles it.

## 6. Tests

- `test_volume.rs` builds a volume in memory: MDB, bitmap, an extents overflow tree, a catalog with nested folders, a file with data and resource forks, a file fragmented into more than three extents (so the overflow tree is exercised), a catalog that itself has more than three extents, an invisible file, a name with every high Roman byte.
- Unit tests per parser with one valid and several malformed fixtures (key length 0, name length 32, type 9, record shorter than its type needs).
- End to end: add classic HFS inside an APM disk, bare, and inside DiskCopy 4.2 to `end_to_end.rs`, and to the damage sweeps.
- `scan` equals a recursive walk.
- Fuzz targets `hfs_mount` (mount, walk, read every file up to a cap, scan) and `dc42`; seeds from the builder.
- A real fixture: a 1440K floppy image made with `hformat`/`hcopy` from hfsutils, a few hundred KB compressed, under `crates/lib/tests/fixtures/` (S, on Linux or WSL), with a test that checks its listing against a checked-in manifest.

## 7. Tasks, in order

| # | Owner | Task | Files owned |
|---|---|---|---|
| 1 | Lead | **Done** (see "Verification" at the top): §2 offsets checked, nothing changed | this file |
| 2 | H | Stubs: `fs/hfs/{mdb,extents,catalog,encoding,test_volume}.rs`, `tools/mac_encodings/`; `Content::Mfs` | those, `fs/probe.rs` |
| 3a | S | §4 encodings and the generator tool | `fs/hfs/encoding.rs`, `encoding/tables.rs`, `tools/mac_encodings/` |
| 3b | S, parallel | `mdb.rs` | `fs/hfs/mdb.rs` |
| 3c | S, parallel | §5 DiskCopy 4.2 | `image/dc42.rs`, `image/mod.rs`, fuzz target |
| 4 | O | `ForkMap::from_runs`; `hfs/extents.rs` | `hfsplus/extents.rs`, `fs/hfs/extents.rs` |
| 5 | O | `hfs/catalog.rs`, `hfs/mod.rs`, probe registration | those, `fs/probe.rs` |
| 6 | S | `test_volume.rs`, end-to-end and sweep additions, fuzz target, the hfsutils fixture | tests, `fuzz/` |
| 7 | S | App: `local_times` in `format.rs`; Roman in the text preview and bytes text column | `format.rs`, `preview/text.rs`, `views/bytes.rs` |

What Julian tries: a System 7 floppy image (DiskCopy 4.2 and raw), a classic Mac CD image with an APM, a hybrid CD; names with accents and bullets; dates that match what a classic Mac showed; a resource fork in Quick Look; copy out with forks.

## 8. Out of scope

MFS reading, per-file script switching, multi-byte scripts, the desktop database, alias resolution, bad-block sparing, the old 16-bit "sparing" of HFS floppies with tags.
