# M8. Wider formats and extension

Four separate pieces of work that share a milestone: more filesystems, the provider protocol (plugins and the elevated helper), the remaining interface features, and packaging. They can be done in any order; the suggested order is formats by usefulness (FAT, exFAT, ISO 9660, NTFS, ext4, UDF), then interface, then the protocol, then packaging.

As in the other designs, layout tables were written from memory of the specifications: **verify each against its primary source before coding** (Microsoft's FAT specification `fatgen103`, the exFAT specification, ECMA-119 for ISO 9660, ECMA-167 and the OSTA UDF specification).

## 1. Crates or in-house

The test for a crate: read-only use is possible; it takes a `Read + Seek` (or positional) source so it can sit on a `BlockSource`; MIT, Apache-2.0, BSD or similar; released in the last 18 months; fuzzed or at least written to return errors on bad input; and it can list a 100,000-entry folder without holding it all per entry in the heap.

| Format | Choice | Why |
|---|---|---|
| FAT12/16/32 | In-house, S | The format fits on two pages; existing crates are read-write and bring write paths we must never call |
| exFAT | In-house, S | Same; the crates are young |
| ISO 9660 with Joliet and Rock Ridge | In-house, S | Simple; no crate covers hybrid Mac discs the way the probe order needs |
| NTFS | The `ntfs` crate, behind `Guarded` | A correct NTFS reader (attribute lists, runlists, index trees, compression) is weeks of O time; this crate is read-only by design and MIT/Apache |
| ext2/3/4 | The `ext4-view` crate, behind `Guarded` | Read-only by design, no `unsafe`, MIT/Apache |
| UDF | In-house, O, last | No usable crate; scope kept to what optical discs and common flash use |

At M8 start the lead checks each named crate (latest version, release date, licence, open panics or soundness issues, whether it still takes `Read + Seek`) and brings the two to Julian for approval (README decision 11). If a crate fails the test, that format drops to "in-house, O, later" and M8 ships without it; the design below for the adapter still stands for whichever passes.

**Crate check (Fable, 2026-11-12, from the crates.io registry).** This is the lead's half of task 1; the approval is Julian's.

| Crate | Newest | Released | Licence | Verdict |
|---|---|---|---|---|
| `ext4-view` | 1.0.0 | 2026-09-14 (five releases in the 20 months before it) | MIT OR Apache-2.0 | **Passes.** Minimum Rust 1.85, which the workspace's edition 2024 already needs. It reads through its own positional-read trait rather than `Read + Seek`, which fits `BlockSource` more directly than `SeekReader` does: implement that trait over an `Arc<dyn BlockSource>` and skip the cursor. Confirm the trait's name and shape in its documentation when the task starts. Recommended: approve. |
| `ntfs` | 0.4.0 | 2023-06-13, and nothing since | MIT OR Apache-2.0 | **Fails the 18-month test** by a wide margin: no release in over three years. By the rule above NTFS drops out of M8. |

What to do about NTFS is Julian's call (README decision 15). The two honest options:

- **Wait (the rule as written, and the recommendation).** M8 ships FAT, exFAT, ISO 9660, ext and UDF. NTFS becomes an in-house reader with its own design after M8. On Windows, where most NTFS disks are met, the host already reads them through "This computer"; what is missing is NTFS inside disk images and on raw disks from other machines.
- **Use it anyway, on probation.** The crate is read-only by design and widely downloaded, and a parser for a format that stopped changing does not rot the way other code does. If Julian prefers this, the gate is: the `ntfs_mount` fuzz target runs for one hour on Linux with no hang and no memory growth before the format is switched on (`guarded` catches a panic; it cannot catch an endless loop or an allocation of a hostile size), and the adapter adds a per-operation deadline around directory walks. A failure of that gate sends it back to "wait".

Neither crate is declared in `Cargo.toml` until Julian says yes.

### 1.1 Adapting a crate

```rust
// fs/adapter.rs
/// `Read + Seek` over a block source, for readers that want a cursor.
pub(crate) struct SeekReader { source: Arc<dyn BlockSource>, pos: u64 }

/// Runs `f`, turning a panic into `Error::Corrupt`. Third-party parsers may
/// panic on hostile bytes; ours may not, so this wraps *only* foreign code.
pub(crate) fn guarded<T>(what: &'static str, f: impl FnOnce() -> Result<T>) -> Result<T>;
```

- `SeekReader::read` clamps at the end (short read, then 0), as `Read` requires; `seek` past the end is allowed and reads then return 0; offset arithmetic is checked.
- `guarded` uses `std::panic::catch_unwind` with `AssertUnwindSafe`; the release profile keeps unwinding for exactly this. After a caught panic the filesystem object is marked poisoned (an `AtomicBool`) and every later call returns the same error: a crate's internal state after a panic is not to be trusted.
- A crate that wants `&mut` access gets one reader per operation (they are cheap: an `Arc` and an offset) or sits behind a `Mutex` when its state is expensive (NTFS's parsed boot sector and MFT location are plain data and are cloned).
- Third-party errors map to `Error::Corrupt` with `CorruptKind::Other` and the structure name "NTFS volume"/"ext volume", keeping the crate's message in a new `CorruptKind::Foreign(String)`.
- `scan` uses the trait's default walk for both crates in M8. An MFT-order scan for NTFS is a later optimization and needs its own design.
- Each adapted format gets a fuzz target that mounts, walks and reads, so the guard is exercised.

`NodeId` for NTFS is the MFT record number (low 48 bits of the file reference); for ext it is the inode number. Hidden: NTFS attribute hidden or system, and names starting with `$` in the root; ext: leading dot. `resource_forks: false`. NTFS alternate data streams are reported through `xattrs`/`read_xattr` under their stream names.

## 2. FAT12, FAT16, FAT32 (`fs/fat/`) — S

Little-endian. Boot sector (sector 0):

| Offset | Size | Field |
|---|---|---|
| 11 | 2 | bytes per sector (512, 1024, 2048, 4096) |
| 13 | 1 | sectors per cluster (power of two, 1–128) |
| 14 | 2 | reserved sectors (≥ 1) |
| 16 | 1 | number of FATs (≥ 1) |
| 17 | 2 | root entries (FAT12/16; 0 on FAT32) |
| 19 | 2 | total sectors, 16-bit (0 → use the 32-bit field) |
| 22 | 2 | sectors per FAT, 16-bit (0 on FAT32) |
| 32 | 4 | total sectors, 32-bit |
| 36 | 4 | sectors per FAT, 32-bit (FAT32) |
| 44 | 4 | root cluster (FAT32) |
| 43 / 71 | 11 | volume label (FAT12/16 at 43, FAT32 at 71) |
| 510 | 2 | `55 AA` |

- Root directory sectors = `ceil(root_entries × 32 / bytes_per_sector)`. First data sector = reserved + FATs × sectors per FAT + root directory sectors. Cluster count = (total sectors − first data sector) / sectors per cluster. **The type is decided by the cluster count alone:** below 4,085 FAT12; below 65,525 FAT16; else FAT32.
- FAT entry for cluster `n`: FAT12 is 12 bits at byte `n + n/2` (odd `n`: high 12 bits of the 16-bit word; even: low 12); FAT16 a u16 at `2n`; FAT32 a u32 at `4n` with the top 4 bits masked off. End of chain: ≥ `0xFF8`, `0xFFF8`, `0x0FFF_FFF8`. Bad cluster: `0xFF7` and equivalents. Free: 0.
- **Chains** are followed with a bound of `cluster_count` steps and a visited check is unnecessary given the bound; a next value below 2 or above the last cluster ends the chain as `Corrupt`. A file's chain is turned into a run list at `open`, capped by `ceil(size / cluster_size)` clusters.
- The whole FAT is read into memory at mount when it is at most 64 MiB (it nearly always is); otherwise entries are read through the cache.
- **Directory entries** are 32 bytes: name 8+3 at 0 (space padded; first byte `0x00` = end of directory, `0xE5` = deleted, `0x05` = a real `0xE5`), attributes at 11 (`0x01` read-only, `0x02` hidden, `0x04` system, `0x08` volume label, `0x10` directory, `0x0F` = long-name entry), creation time and date at 14 and 16, modified time and date at 22 and 24, first cluster high u16 at 20 (FAT32) and low u16 at 26, size u32 at 28.
- **Long names:** entries with attribute `0x0F` precede their short entry in reverse order: sequence byte at 0 (`0x40` marks the last, which comes first), UTF-16LE units at 1–10, 14–25, 28–31 (13 per entry), checksum of the short name at 13. Collect up to 20 entries (255 units); use the long name only when the sequence is complete and the checksum matches (`sum = ((sum >> 1) | (sum << 7)) + byte` over the 11 short-name bytes, in `u8` wrapping arithmetic); otherwise fall back to the short name. Short names: bytes through the OEM code page; use code page 437 for bytes ≥ 0x80 (a 128-entry table, S generates it from the Unicode.org `CP437.TXT`), lowercase per the case bits at byte 12 (`0x08` base, `0x10` extension).
- Dates: local time, 2-second resolution: date = `(year − 1980) << 9 | month << 5 | day`, time = `hour << 11 | minute << 5 | seconds / 2`. Set `local_times`. Invalid fields (month 0, day 0) → `None`.
- `NodeId`: the byte offset of the entry's short-name record on the volume (unique and lets `metadata` re-read it); the root is `NodeId(0)` on FAT12/16 and the root cluster's first byte offset on FAT32. `.` and `..` are skipped. A directory's own chain bound: `MAX_DIR_BYTES = 256 MiB`.
- `lookup`: walk the directory, compare long names case-insensitively (`char::to_lowercase`) and short names likewise.
- `allocation_map`: from the in-memory FAT (entry non-zero = used). `extents`: the chain as merged byte runs. `identity`: the volume serial (4 bytes at 39 or 67) and the label; `change`: none reliable, so `identity()` returns `None` and the index is rebuilt each session.
- `info.format`: "FAT12", "FAT16" or "FAT32". The probe already recognises FAT; keep `Content::Fat`.

## 3. exFAT (`fs/exfat/`) — S

Boot sector: `EXFAT   ` at 3; partition offset u64 @64; volume length u64 @72 (sectors); FAT offset u32 @80; FAT length u32 @84; cluster heap offset u32 @88; cluster count u32 @92; root directory first cluster u32 @96; serial u32 @100; flags u16 @106; bytes-per-sector shift u8 @108 (9–12); sectors-per-cluster shift u8 @109 (sum of both shifts ≤ 25).

- Cluster `n` (≥ 2) is at `(heap offset + (n − 2) << spc shift) << bps shift`. FAT entries are u32; end of chain `0xFFFF_FFFF`, bad `0xFFFF_FFF7`.
- Directory entries are 32 bytes; byte 0 is the type; bit 7 clear = unused (0 = end of directory). Types: `0x81` allocation bitmap (first cluster u32 @20, length u64 @24), `0x82` up-case table (ignored: compare with `char::to_lowercase`), `0x83` volume label (length u8 @1, UTF-16LE @2), `0x85` file, `0xC0` stream extension, `0xC1` file name.
- A file is a set: one `0x85` (secondary count u8 @1, attributes u16 @4 with the FAT meanings, created u32 @8, modified u32 @12, accessed u32 @16, 10 ms increments @20–21, UTC offsets @22–24), one `0xC0` (flags u8 @1: bit 1 = **no FAT chain, the file is contiguous**; name length u8 @3; valid data length u64 @8; first cluster u32 @20; data length u64 @24), then `ceil(name length / 15)` `0xC1` entries (15 UTF-16LE units each from byte 2). Bound: secondary count 2–18; a set that is malformed is skipped, the walk continues.
- A contiguous file's extent is one run of `ceil(length / cluster size)` clusters, without consulting the FAT. Reads past the valid data length and inside the data length return zeros.
- Timestamps: the FAT date/time packed into a u32 (date in the high 16 bits), plus the UTC offset byte (bit 7 set = valid, low 7 bits signed quarter-hours): when valid, convert to UTC and leave `local_times` handling per entry out by reporting UTC; when not valid, treat as UTC and accept the error (exFAT volumes written by cameras rarely set it). `local_times: false`.
- `NodeId`: byte offset of the `0x85` entry. Root: the root cluster's byte offset. `allocation_map`: the bitmap file. `identity`: `None`.

## 4. ISO 9660 (`fs/iso9660/`) — S

2048-byte sectors (the image may have been made with others; the logical block size field says). Volume descriptors start at sector 16: type u8 @0, `CD001` @1, version @6; type 1 primary, 2 supplementary (Joliet), 255 terminator. Scan at most 64 descriptors.

- Primary descriptor: volume id @40 (32 bytes, space padded), volume space size @80 (both-endian u32: little-endian first), logical block size @128 (both-endian u16), root directory record @156 (34 bytes).
- Directory record: length u8 @0 (0 = padding to the next sector), extent location @2 (both-endian u32), data length @10 (both-endian u32), date @18 (7 bytes: years since 1900, month, day, hour, minute, second, offset in quarter-hours signed), flags @25 (`0x01` hidden, `0x02` directory, `0x80` continues in the next record: multi-extent file), name length @32, name @33, then padding to even, then the system-use area to the record's end.
- Names: strip `;1` version suffixes and a trailing `.`; names `0x00` and `0x01` are `.` and `..` (skipped).
- **Joliet:** a supplementary descriptor whose escape sequences @88 are `%/@`, `%/C` or `%/E`; its tree has UCS-2 big-endian names. **Rock Ridge:** system-use entries in the primary tree: `NM` (name; flag bit 0 = continues), `PX` (mode, links, uid, gid), `SL` (symlink components), `TF` (timestamps), `CE` (continuation area: block, offset, length — follow at most 8 per record), `RE`/`CL`/`PL` (relocated directories: honour `CL` by listing the child at its real location, hide `RE` entries). Signature at offset 0, length u8 @2, version @3.
- **Which tree:** Rock Ridge when the root's `.` record carries an `SP` entry; else Joliet when present; else plain ISO names (uppercase, shown as stored).
- Directories are contiguous extents; walk records sector by sector (a record never crosses a sector). Bounds: directory length ≤ `MAX_DIR_BYTES = 64 MiB`; depth of the default scan guarded by the visited set (ISO images can contain loops by construction).
- Multi-extent files: consecutive records with the same name and flag `0x80` on all but the last are one file; its extents are concatenated.
- `NodeId`: byte offset of the directory record. `info.format`: "ISO 9660". Dates carry an offset, so they convert to UTC properly.
- A hybrid Mac disc is named for its HFS side by the probe order; the user sees the Mac view. A way to choose the ISO view of a hybrid disc is a "View as" menu on the volume: record it as a follow-up, not M8.

## 5. UDF (`fs/udf/`) — O, last

Scope for the first version: UDF 1.02 to 2.60 on images and discs written in one session; type 1 (physical) partitions; the metadata partition of 2.50 and 2.60 (Blu-ray, most flash); short and long allocation descriptors and data embedded in the file entry. Not in scope: VAT (packet-written CD-R), sparable partitions (CD-RW), multi-session discs, named streams, UDF encryption. Each is detected (partition map type) and reported as unsupported by name.

Path from the start, with every tag verified (tag id u16 @0, checksum u8 @4 over the 16 tag bytes except byte 4, CRC-16 CCITT @8 over CRC length @10 bytes after the tag, tag location u32 @12 equal to where it was read):

1. Anchor volume descriptor pointer (tag 2) at sector 256, else last sector − 256, else the last sector. Sector size: try 2048, then 512, then 4096.
2. It gives the main volume descriptor sequence extent (length u32, location u32); read at most 64 descriptors: partition descriptor (tag 5: number @22, start @188, length @192), logical volume descriptor (tag 6: block size @212, file set descriptor location @248 as a long allocation descriptor, partition maps count @268 and maps from 440), terminating descriptor (tag 8). Fall back to the reserve sequence when the main one fails.
3. Partition maps: type 1 (6 bytes: volume sequence u16, partition number u16) or type 2 (64 bytes, identified by its entity id string: `*UDF Metadata Partition` → metadata file location u32 @40 within the partition it names; `*UDF Virtual Partition` and `*UDF Sparable Partition` → unsupported).
4. With a metadata partition: read the metadata file's file entry at that location and map its extents; logical blocks in that partition are then offsets into the metadata file.
5. File set descriptor (tag 256) → root directory ICB (long allocation descriptor @400).
6. File entry (tag 261) or extended file entry (tag 266): ICB tag at 16 (file type u8 @11 of the ICB tag: 4 directory, 5 file, 12 symlink; flags u16 @18: low 3 bits = allocation descriptor form, 0 short, 1 long, 3 embedded), information length u64 @56, modification time @84 (file entry; differs in the extended form), length of extended attributes u32 @168 and of allocation descriptors u32 @172 (file entry; 208 and 212 in the extended form), then the descriptors. Short descriptor: length u32 (top 2 bits: type; 0 recorded, 1 allocated not recorded = zeros, 3 = continuation extent), position u32. Long: length u32, block u32, partition u16, 6 bytes.
7. A directory's data is a run of file identifier descriptors (tag 257): characteristics u8 @18 (`0x01` hidden, `0x02` directory, `0x04` deleted, `0x08` parent), identifier length u8 @19, ICB (long descriptor) @20, implementation-use length u16 @36, then the implementation use, then the identifier, padded to 4 bytes. Names are OSTA compressed Unicode: first byte 8 → Latin-1 bytes, 16 → UTF-16 big-endian.

Bounds: descriptor sequences ≤ 64; allocation descriptors per file ≤ 1,000,000 with continuation extents ≤ 4096; directory data ≤ 256 MiB; every extent inside its partition. `NodeId`: partition reference number in the top 16 bits, logical block of the file entry in the low 48. Timestamps carry a timezone field; convert to UTC.

Because this is the one hard format in M8 and discs written by different tools disagree, it needs real fixtures before O starts: a DVD-ROM image (UDF 1.02 bridge with ISO 9660), a Blu-ray data image (2.50), a UDF-formatted USB stick from Windows and from macOS (`newfs_udf`). Julian or the lead gathers them first.

## 6. Registration, common to all formats

- `probe::mount` gains an arm per format (the exhaustive match forces it). `Content` already has every variant.
- Each format: a module with `mount(source) -> Result<Fs>`, a `test_volume.rs` builder where the format is in-house, unit fixtures, a fuzz target `<fmt>_mount`, an entry in `end_to_end.rs` and the damage sweeps, a paragraph in `filesystems.md`, and the support table there updated.
- None sets `resource_forks`. The disk strip, search (default scan), Quick Look and copy work without app changes; that is the test that the trait is right. If any format needs an app change, stop and raise it.

## 7. The provider protocol — the wire details are in `provider-protocol.md`; O builds the host side

**The byte-level specification is `provider-protocol.md` in this folder (Fable, 2026-11-12; this was task 10).** It is the reference for task 11 and replaces §7.2 and §7.3 below wherever they differ. The differences, so nobody builds the older sketch: scans are *pulled* (`ScanOpen`, `ScanNext`, `ScanClose`, like a folder listing) instead of pushed as a sequence of frames, so there is no `Cancel` message and no reply stream; the host's reader thread never writes; `VolumeInfo::format` names come from the extension's manifest; a method an extension lacks falls back to the trait's default. §7.1, §7.4 and §7.5 stand, with the helper's channel per platform and its checks spelled out in the specification's §8.2.

"Extensions that are not compiled in" (the README's goal) and "a small read-only helper process" for elevation (`architecture.md`) are the same mechanism pointed in two directions: two processes joined by a pipe, one serving block reads to the other.

### 7.1 Why processes

| Option | Verdict |
|---|---|
| Dynamic libraries | Needs `unsafe`, has no stable Rust ABI, and a plugin bug or malicious plugin runs inside the app with its rights. No. |
| WebAssembly | Safe, but a runtime is a very large dependency for a feature few will use, and block reads across the boundary are slow to do well. Not now. |
| **Child process over pipes** | No `unsafe`, any language, a crash is a closed pipe, and the host keeps the device handle. Cost: a round trip per call (tens of microseconds), which paged listings and bulk reads absorb. **Chosen.** |

The central rule: **a filesystem plugin never opens the disk.** It asks the host for bytes. So a plugin cannot write to a device, needs no elevation, and can be run with whatever sandboxing the platform offers later.

### 7.2 Framing (`crates/lib/src/ipc/`)

- A frame: `length u32 LE` (of what follows), `request id u32`, `kind u16`, `flags u16`, payload. `MAX_FRAME = 16 MiB + 64`. A longer length is a protocol error and ends the session.
- Integers little-endian; strings `u32` length + UTF-8; byte arrays `u32` length + bytes; options a `u8` tag; results a `u8` tag then the value or an error (code `u16`, message string). Hand-written encode and decode with the same checked readers as the parsers; no `serde`. Both sides treat the other as hostile: every length checked, every enum tag checked.
- Requests are answered in any order, matched by id. Either side may send requests (the plugin sends `Read`, the host sends everything else), so each side runs one reader thread that routes replies to waiting callers through a `Mutex<HashMap<u32, Sender>>` and hands incoming requests to a small worker pool.
- Handshake: each side sends `Hello { protocol_version: u16 = 1, role: u8, name: string }` and closes on a version it does not speak.

### 7.3 Messages

Host → filesystem provider:

| Kind | Payload | Reply |
|---|---|---|
| `Probe` | source id, length, sector size, first 64 KiB | `Option<format name>` |
| `Mount` | source id, length, sector size | volume handle u32 + `VolumeInfo` |
| `Metadata`, `Lookup`, `Extents`, `Xattrs`, `ReadXattr`, `AllocationMap`, `Identity` | volume handle + the trait method's arguments | the trait method's result |
| `OpenDir` | volume, node | stream handle |
| `NextBatch` | stream handle, max | a `DirBatch` (names buffer + records) + progress |
| `Open` | volume, node, stream | file handle + length |
| `ReadFile` | file handle, offset, length (≤ 16 MiB) | bytes |
| `Scan` | volume | a sequence of `ScanChunk` frames (each a batch of entries) ending with a summary; the host may send `Cancel(request id)` |
| `Close` | any handle | — |

Provider → host: `Read { source id, offset, length ≤ 16 MiB }` → bytes; `Log { level, text }`.

The host side wraps this as `RemoteFs: FileSystem` (and `RemoteDir: DirStream`, `RemoteFile: BlockSource`), so the app cannot tell a plugin volume from a built-in one. A provider that dies or breaks protocol turns every call into `Error::Io` with "the <name> extension stopped", and the source becomes `Failed`.

Timeouts: 30 s per request (a hung plugin must not hang a background thread forever), 5 s for the handshake.

### 7.4 Discovery and trust

- Plugins live in `<config dir>/extensions/<name>/` with a `manifest` file (`key = value`: `name`, `version`, `command`, `formats`). Nothing is loaded from anywhere else, and nothing is downloaded by the app.
- A plugin is off until the user turns it on in Settings, which shows its name, path and the sentence "This extension is a separate program. It can read the disks you open but cannot change them." (True by construction: reads go through the host.)
- Plugin probes run after all built-in probes and only on content the built-ins call `Unknown`.
- A reference plugin lives in the repository (`plugins/example-romfs/` or similar, a tiny format) to keep the protocol honest, built in CI.

### 7.5 The elevated helper

The same framing, roles reversed: the app starts `disk-atlas --device-helper` elevated (UAC on Windows; `pkexec` on Linux; `osascript ... with administrator privileges` on macOS) with the pipe's name or inherited handles as arguments. The helper serves exactly three requests: `ListDevices`, `OpenDevice(path)` (the path must be one that `list()` returned), `Read { handle, offset, length }`. It opens devices read-only, never touches the filesystem otherwise, and exits when the pipe closes. The app wraps it as `RemoteDevice: BlockSource` under the usual `CachedSource`. Then the main process never needs to restart elevated, and "Restart as administrator" becomes "Allow reading disks".

This replaces relaunching, and is security-sensitive (a privileged process taking requests): the pipe must be private to the two processes (anonymous pipes inherited by the child on Unix; on Windows a named pipe with a random name, `FILE_FLAG_FIRST_PIPE_INSTANCE`, and an ACL for the current user only, or anonymous handles passed through `ShellExecuteExW` if inheritance across elevation proves workable — **this needs investigation with real UAC before design is final**). It is `unsafe` platform code: O, with Fable's review, and Julian's explicit go-ahead as rule 2 of `CLAUDE.md` requires.

Order: build framing and `RemoteFs` first with the example plugin (all safe code), then the helper.

## 8. Interface — S

### 8.1 Settings (`state/settings.rs`, `views/settings.rs`)

A `Settings` struct with defaults, loaded at startup before the window opens (a small synchronous read of one file is fine there and nowhere else), saved on change through a background write to a temp file then rename.

File: `<config dir>/settings` (`%APPDATA%\DiskAtlas`, `~/Library/Application Support/DiskAtlas`, `$XDG_CONFIG_HOME/disk-atlas`). Format: one `key = value` per line, `#` comments, unknown keys kept and written back, bad values ignored with a logged warning. No dependency.

| Key | Values | Default |
|---|---|---|
| `theme` | `system`, `day`, `night` | `system` |
| `view` | `list`, `icons` | `list` |
| `show_hidden` | bool | `false` |
| `details_pane` | bool | `true` |
| `sidebar` | bool | `true` |
| `keep_forks_on_copy` | bool | `false` |
| `last_copy_destination` | path | none |
| `cache_megabytes` | 64–4096 | 256 |
| `save_search_indexes` | bool | `true` |
| `pinned` | repeated key: `<source identity hex>:<node id>:<name>` | none |
| `recent_image` | repeated key: path (most recent first, 10) | none |
| `extension.<name>` | bool | `false` |
| `hfs_encoding.<identity hex>` | an `Encoding` name | Roman |

The settings view is a floating panel like Quick Look (Ctrl+, to open): Appearance, Browsing, Copying, Search (with "Delete saved indexes" and their total size), Extensions. Plain rows, the existing control styles.

### 8.2 Theme switch

`Theme` is already a GPUI global with day and night colors. Add `ThemeChoice` from settings; `system` follows `window.appearance()` and its change observer (verify the names in 0.2.2). Switching is instant and repaints once.

### 8.3 Icon view

Ctrl+2. The same `Listing` state, selection and keyboard model; only the view differs. A virtualized grid: `uniform_list` over *rows of cells*, with the column count computed from the width (cell 96 × 96 at the default size; three sizes: 64, 96, 128). Arrow keys move in two dimensions (`Motion` gains `Left`, `Right`, and Up/Down move by the column count: implement in `Listing::move_cursor` with a `columns` argument so selection logic stays in one place). Each cell: a kind icon, or a thumbnail for images through the Quick Look cache (decode at most `2 × cores` at a time, cancel when scrolled away, 64 MiB cache, never the accent), and the name on two lines, middle-truncated. Sorting applies as in the list. No free positioning.

### 8.4 Pinned folders

A fourth sidebar group, "Pinned", above Disks. Pin with Ctrl+D or the context menu on a folder; unpin the same way. A pin stores the volume identity, the node id and the display name. When its volume is mounted the pin is live and navigates there (verify the node still exists and is a folder through `metadata`; if not, try the stored path by `lookup` from the root; else show it struck through with "This folder is no longer there"). When its volume is not present the pin is dimmed and its tooltip names the volume. Host folders pin by path. Volumes without an identity cannot hold pins (the menu item is disabled with that reason).

### 8.5 Remaining keys

Ctrl+L (path as text): the breadcrumb turns into the text input holding the path with `/` separators; Enter resolves it by `lookup` segment by segment on the background executor and navigates to the deepest folder found, reporting the first segment that failed; Escape restores the breadcrumb. Update the keyboard table in `ui-design.md`.

## 9. Packaging — S, with Julian for signing

| Platform | Artifact | Notes |
|---|---|---|
| Windows | A zip of `disk-atlas.exe` and the licence files, and an MSI or Inno Setup installer | Embed an application manifest (per-monitor DPI awareness v2, `asInvoker`: the app itself never asks for elevation at start). Icon and version resources through a `build.rs` using a resource compiler crate, which is a build dependency needing approval. Code signing needs a certificate: Julian's decision and cost. |
| Linux | AppImage and a `.deb`; a `.desktop` file, icons, and a MIME entry for `.dmg`, `.sparseimage`, `.img`, `.iso` | A polkit policy file only once the helper exists. Flatpak is possible later; raw device access through the helper does not fit its sandbox. |
| macOS | A `.app` bundle in a `.dmg` | `Info.plist` with document types for the image formats. Notarization needs an Apple developer account: Julian's decision. Without it, the README explains the right-click-open step. |

- A release workflow in `.github/workflows/release.yml`, triggered by a tag, builds the three with `--release`, attaches artifacts and SHA-256 sums. It does not publish anywhere else.
- Fonts and their OFL licence, the GPL text and a `THIRD-PARTY-LICENSES` file (generated with `cargo about` or `cargo license` in CI; the tool is not a dependency of the crates) ship in every artifact.
- The version comes from the workspace `Cargo.toml`. "About Disk Atlas" in the settings panel shows it, the licence, and the source link (GPL section 6 wants the offer of source to be easy to find).
- Before the first public build: run the fuzz targets for an hour each, and the full test suite in release mode on all three platforms.

## 10. Tasks

Formats (each line is: stubs H, then the owner, then the lead's review and a real image from Julian):

| # | Owner | Task |
|---|---|---|
| 1 | Julian | Approve `ext4-view`; decide NTFS (the check itself is done, §1) |
| 2 | S | FAT (§2) |
| 3 | S | exFAT (§3) |
| 4 | S | ISO 9660 with Joliet and Rock Ridge (§4) |
| 5 | S | `fs/adapter.rs`; NTFS through the crate; ext through the crate |
| 6 | O | UDF (§5), after fixtures exist |

Interface: 7 settings and theme (S); 8 icon view (S for the view, O reviews the grid's virtualization); 9 pins and Ctrl+L (S).

Protocol: 10 **done**: the byte-level specification is `provider-protocol.md`; 11 O builds `ipc/` and `RemoteFs` from it, with its §9 conformance tests; 12 S builds the example plugin and its CI job; 13 extensions in Settings (S); 14 the elevated helper: the lead runs the four checks at the end of the specification's §8.2 with a real prompt (Julian at the keyboard), corrects §8.2 from what they show, then O builds it, with Julian's go-ahead. If check (1) fails (the elevated helper cannot open the app's pipe), that is a stop-and-ask.

Packaging: 15 S per platform; 16 Julian decides on signing and notarization.

## 11. Out of scope

Write support of any kind; network filesystems; RAID, LVM, Core Storage, BitLocker, LUKS and other volume layers (each is a `BlockSource` over other sources and would get its own design); ZFS, Btrfs, XFS, ReFS (plugins are the intended route); automatic updates.

## Changes during build

2026-11-16, FAT and exFAT (lane C):

- **FAT root node id.** The design makes the FAT32 root the first byte offset of the root cluster, but the first entry of the root directory is at that same offset, so a file there would share the root's id. The FAT32 root is the root cluster's first byte plus one; entry offsets are multiples of 32, so no entry can have it. exFAT does the same.
- **FAT `identity`.** `None`, as designed. The volume serial is not used.
- **exFAT entry sets that cross a cluster.** A node id is the offset of the `0x85` primary. When the primary is the last slot of a cluster of a directory, its `0xC0` stream slot is in the directory's next cluster, which a bare offset cannot find (a FAT-chained directory's clusters need not be adjacent). The listing and `lookup` remember such sets (stream slot offset and parsed meta, at most 1,048,576), so `metadata` and `open` answer for any node that was listed or looked up. A node id invented without listing its folder, which happens to be a crossing primary, is `NotFound`. Sets that do not cross are read from the offset alone.
- **exFAT free space.** `info.free_bytes` is counted from the allocation bitmap at mount only when the bitmap is at most 1 MiB (volumes up to about 8M clusters); otherwise `None`. `allocation_map` reads the whole bitmap in 64 KiB pieces.
- **exFAT set checksum.** Not verified. A set with a wrong checksum lists as written; sets with a bad secondary count, a first secondary that is not a stream extension, a name entry of the wrong type, a zero name length or too few name entries are skipped.
- **Free-space and times.** FAT `free_bytes` counts zero entries in the in-memory FAT (`None` when the FAT is over 64 MiB). exFAT times are reported in UTC when the entry carries a valid offset, else as stored, and `local_times` is false; FAT times are local, `local_times` is true.

