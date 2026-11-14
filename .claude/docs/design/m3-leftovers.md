# M3 leftovers and M2 devices

Three things are open before M4: the conformance pass, device access on Linux and macOS, and a few small debts. None of them blocks M4; they can run beside it.

## 1. Conformance pass: what each answer changes

Julian runs `tools/conformance/manifest.sh` on a Mac and `atlas_check` on the same images (see `tools/conformance/README.md`). The lead compares the two outputs. For each open question, the table gives the observation to look for and the exact change. Every change is small and belongs to S unless marked.

### 1.1 UDIF `mish` data offset

Look at: any real `.dmg` whose `blkx` entries have a non-zero `mish` data offset (bytes 24..32 of the `mish` block, big-endian u64) or whose `koly` data fork offset is non-zero. `atlas_check --read-data` either matches the manifest hashes or fails zlib decoding.

| Observation | Change |
|---|---|
| All real images have both fields 0 | Nothing. Record in `filesystems.md` that no image tested exercises it. |
| Field non-zero and files read correctly | Nothing; delete the "not yet shown" sentence. |
| Field non-zero and reads fail or hashes differ | In `image/udif.rs`, `chunk_file_offset`: drop the `mish` data offset from the sum (keep the data fork offset). Re-run. If that also fails, try the reverse (drop the fork offset). Add the image's header bytes as a unit fixture. |

### 1.2 Sparse images past 1008 bands

Need: a `.sparseimage` with more than about 1 GiB written (`hdiutil create -size 4g -type SPARSE -fs HFS+`, then copy 2 GiB in). Dump bytes 0..4096 and the 4096 bytes at each offset where a second table could start.

Expected layout (from libmodi's later notes, unconfirmed): the header's field at byte 32 is the file offset of the next index node; an index node is a 4096-byte block with signature `shdw` or a repeat of `sprs`, its own next-pointer, and a continuation of the band table; slots after the first table are numbered on, and their data sits after the node. O does this change because a wrong guess misreads data silently.

| Observation | Change |
|---|---|
| Byte 32 is a file offset to a block with a recognisable signature and a table | Implement the chain in `image/sparseimage.rs`: bound it by `file_len / 4096` nodes, reject a node offset that is not past the previous one, keep the "data past the last mapped slot is refused" rule for the final table. Add the dumped headers as fixtures and a fuzz seed. |
| Anything else | Do not guess. Keep the refusal, put the hex dump in `filesystems.md`, raise it to Fable. |

### 1.3 Sparse bundle backing store version 2

Need: `Info.plist` and a directory listing of a bundle made by current macOS.

| Observation | Change |
|---|---|
| Same keys, bands directly in `bands/` named in hex | Accept version 2 in `image/sparsebundle.rs` with the version 1 reader. |
| Bands nested in subfolders (for example `bands/0/1f`) | Accept version 2; change band path building to the observed scheme (a function of the band number only); keep version 1 as is. A missing band still reads as zeros. |
| A `mapped/` directory or other new files that hold data | Refuse as now; record the listing; raise it to Fable. |

### 1.4 `.hotfiles.btree`

| Observation in the macOS manifest | Change |
|---|---|
| Not listed by `ls -a /Volumes/X` | In `hfsplus/mod.rs`, leave it out of the root listing and root `lookup` next to the private folders, by name match on the root's children only. Same for `.journal` and `.journal_info_block` if the manifest hides those too (xnu hides all three by CNID recorded in the volume header's Finder info; a name match in the root is enough for a read-only reader). |
| Listed | Nothing. |

### 1.5 Resource fork of compressed files

| Observation (`stat -f %z file/..namedfork/rsrc`, `ls -l@`) | Change |
|---|---|
| macOS reports 0 and no `com.apple.ResourceFork` | Nothing: this is what M3 implements. |
| macOS reports the compressed fork's size | Still nothing in the engine. `resource_size` stays `Some(0)`: the column is for forks a user made. Note the difference in `filesystems.md`. |

### 1.6 Also compare, no question attached

- Name composition: every name in the manifest equals the name `atlas_check` prints, byte for byte after NFC of the manifest side. A mismatch on a non-BMP or excluded-range character goes to Fable with the stored UTF-16.
- Sizes and modification times of every file; data hashes with `--read-data`.
- Hard link counts: both links show the same size and hash.
- A volume that was not unmounted cleanly (pull the cable on a test stick while copying): the journal state line, and that the listing matches what macOS shows after it replays.

When the pass is done: tick the M3 box, move every "not yet tried against a real image" note in `roadmap.md` to "checked against <image name>", and delete resolved traps from `filesystems.md`.

## 2. Device access on Linux and macOS (O)

The Windows half exists (`io/device/windows.rs`); `DeviceInfo`, `list` and `open` are fixed. These two files are stubs.

### Linux (`io/device/linux.rs`), no FFI, no `unsafe`

- **List.** Read the entries of `/sys/block`. Skip names starting with `loop`, `ram`, `zram`, `dm-`, `md`, `sr` is kept (optical). For each `name`:
  - `size`: `/sys/block/<name>/size`, a decimal count of 512-byte units whatever the sector size. Multiply by 512 with `checked_mul`. Skip the device when it is 0 (an empty card reader).
  - `sector_size`: `/sys/block/<name>/queue/logical_block_size`, decimal; must be a power of two from 512 to 65536, else 512.
  - `removable`: `/sys/block/<name>/removable` is `1`.
  - `model`: `/sys/block/<name>/device/model`, trimmed; absent for NVMe on old kernels, then `None`.
  - `path`: `/dev/<name>`.
  - Each file is read with a 4096-byte cap. A file that is missing or does not parse skips that field, not the device, except `size`.
- **Open.** `File::open` (read-only). `EACCES`/`EPERM` map to `Error::AccessDenied` through `Error::io`. Size is taken again from sysfs at open, not from seeking. Reads use `FileExt::read_at`; a block device accepts unaligned buffered reads, so no bounce buffer is needed, but reads are still split at 16 MiB like Windows.
- Sort the list by name so the order is stable.
- Tests: the sysfs parsers take a root path, so a unit test builds a fake `/sys/block` in a temp dir (present, missing and garbage files).

### macOS (`io/device/macos.rs`)

- **List.** Read `/dev` for names matching `disk<N>` exactly (no `s` suffix: slices are partitions). For each, the size and sector size need two `ioctl`s: `DKIOCGETBLOCKSIZE` (`0x40046418`, u32) and `DKIOCGETBLOCKCOUNT` (`0x40086419`, u64). Verify both constants against `<sys/disk.h>` before use. These need an open descriptor, and opening `/dev/rdiskN` needs root, so an unprivileged process cannot size a disk. `DeviceInfo::size` must not be 0, so a device that cannot be opened is left out. `list` returns the devices it could size, and fails with `Error::AccessDenied` only when it could size none and at least one `diskN` existed. The app shows that as one "needs elevation" row in the Disks group (a small change in `state/sources.rs`, S: today a failed `list` is logged and shows nothing).
- `ioctl` is the only FFI. It needs `libc` (already in `Cargo.lock`; a new direct dependency, so Julian approves it with this task) or a three-line `extern "C"` block. Prefer the `extern` block: one function, two call sites, each under `#[allow(unsafe_code)]` with a `// SAFETY:` comment saying the descriptor is open and the out-pointer is a live, correctly sized local.
- `model` and `removable`: `None` and `false` for now. IOKit is the right source and is not worth the FFI before a Mac user asks.
- **Open.** `/dev/rdiskN` (the raw device). Raw devices need sector-aligned offsets and lengths: reuse the bounce-buffer path from `windows.rs` by moving it into `io/device/mod.rs` as a private `aligned_read(file, sector_size, offset, buf)` helper. That move is part of this task and must keep the Windows tests green.
- This file cannot be tested from Windows. The CI macOS job compiles it; a real run waits for Julian or a contributor with a Mac.

Elevation on both stays as designed in `architecture.md`: the hint text, no button.

## 3. Small debts

| Debt | Owner | Change |
|---|---|---|
| LZMA fixture script lives only in an agent's temp folder | S | Recreate under `tools/lzma_fixtures/` as a small Rust binary crate outside the workspace (like `tools/hfs_unicode/`), using `lzma-rs` to emit the same streams, with a README giving the command and the SHA-256 of each output. The checked-in fixtures must be byte-identical or be replaced together with their expected hashes. |
| In-house LZMA at 75 MB/s, not profiled | O, optional | Only after a real ULMO image shows it matters. Profile with `--profile profiling`; the usual wins are a bulk literal path and copying matches with `copy_within` when distance ≥ length. Report before and after. |
| Deferred details rows: "On disk: N pieces", Kind from type codes, "compressed" in Kind | S | With M5's details work; see `m5-quicklook-copy.md` §8. |
| Things not yet seen in a window (folder button, strip shading, details rows) | Julian | Listed in the M3 summary. Nothing to build. |
