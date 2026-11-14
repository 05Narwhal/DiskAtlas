# On-disk formats

Working notes for implementers. These summarize the specifications; they do not replace them. Where a number matters, confirm it in the primary source listed at the end before writing code, and say so in your report if the source and these notes disagree.

## Support plan

| Format | Read support | Milestone |
|---|---|---|
| HFS+ and HFSX (journaled, case-sensitive, wrapped) | Written here | M3 |
| HFS (classic) | Written here | M6 |
| APFS, unencrypted | Written here | M7 |
| APFS, software-encrypted with password | Written here | after M7 |
| FAT12/16/32, exFAT, NTFS, ext2/3/4, ISO 9660, UDF | Behind the same trait, using vetted crates where they exist | M8 |
| Everything else | Plugin providers | later |

"All disk formats" is the direction, not a promise for a date. Apple formats come first because they are the reason the project exists.

Out of reach by design: internal disks of Macs with a T2 chip or Apple silicon. Their volume keys are bound to the machine's hardware and cannot be unlocked anywhere else. Detect and say so.

## Partition maps

**GPT.** Header at LBA 1, signature `EFI PART`, little-endian. LBA size is the device sector size (512 or 4096), so try both if the header is not found. Entries are usually 128 bytes; read the size and count from the header and bound them. Names are UTF-16LE. GUIDs are mixed-endian (first three fields little-endian). Useful type GUIDs: HFS+ `48465300-0000-11AA-AA11-00306543ECAC`, APFS `7C3457EF-0000-11AA-AA11-00306543ECAC`, Core Storage `53746F72-6167-11AA-AA11-00306543ECAC`, EFI System `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`. The type GUID is a hint; always probe the contents.

**MBR.** Signature `55 AA` at byte 510, four entries from byte 446. Type `0xEE` means protective MBR (read the GPT), `0xAF` is HFS or HFS+, `0x05` and `0x0F` are extended partitions (bound the chain).

**Apple Partition Map.** Big-endian. Block 0 starts with `ER` and holds the block size; partition entries start at block 1 with signature `PM`, and each repeats the total entry count. The block size may be 512 or 2048 (optical media); if block 0 is unusable, try both. Trap: a CD image may say 2048 in block 0 yet place the entries 512 bytes apart, and its fourth entry then sits at byte 2048 and looks like a valid 2048-stride map. `apm.rs` therefore accepts a unit only when the whole chain (as long as the first entry's count) carries `PM` at that stride, and otherwise takes the unit with the longest chain. Types are strings such as `Apple_HFS`.

**No map.** A whole device or image can be a bare filesystem. Probe offset 0 too.

**Core Storage** (Fusion drives, FileVault 2 on HFS+) is a logical volume layer. Detect it and report it as unsupported for now.

## Disk images

- **Raw** (`.img`, `.iso`, `.cdr`, `.dsk`): the file is the disk.
- **UDIF** (`.dmg`): a 512-byte `koly` trailer at the end of the file, big-endian, points to an XML property list. Its `blkx` entries each hold a `mish` block: a table of chunks mapping sector ranges to compressed ranges in the file. Chunk kinds: zero fill, raw, ignore, ADC, zlib, bzip2, LZFSE, LZMA, comment, terminator. Build a sorted chunk index once, decompress per chunk on demand, cache decompressed chunks. Encrypted images have a different outer header; detect and report. Implemented in `image/udif.rs`. Traps and decisions:
  - LZMA chunks (`0x80000008`, ULMO images) are xz streams holding LZMA2, as 7-Zip reads them. lzma-rs cannot bound its memory on hostile input (it buffers whole xz blocks and runs LZMA2 with no memory limit), so the decoder is in-house (`image/udif/lzma.rs`) and writes only into the chunk's output slice. lzma-rs is a dev-dependency, used only to make test streams.
  - A chunk's file offset is taken as data fork offset + `mish` data offset + compressed offset. hdiutil writes the first two as 0, so real images have not yet shown whether the `mish` data offset belongs in the sum; `chunk_file_offset` is the one place to change.
  - Rejected at open: version other than 4, segmented images, images with no property list (very old ones keep `blkx` in a resource fork only), unknown chunk types, overlapping chunks, chunks outside their `mish` block or the data fork. Checksums are not verified. Decoded and stored chunk size are capped at 64 MiB, the property list at 256 MiB.
- **Sparse bundle**: a folder with `Info.plist` (band size, total size) and `bands/` files named in hexadecimal. A missing band reads as zeros. Implemented in `image/sparsebundle.rs`. Only `bundle-backingstore-version` 1 opens; newer macOS writes 2, whose layout we have not seen. A `token` file means encryption (unsupported). `Info.bckup` is the fallback when `Info.plist` cannot be read.
- **Sparse image**: single file, a 4096-byte big-endian header (`sprs`, version 3, sectors per band at byte 8, disk sectors at byte 16) whose table at byte 64 maps file slots to disk bands (1-based, 0 unused). Slot `i` is at `4096 + i × band`. Layout from libyal libmodi's notes. Implemented in `image/sparseimage.rs`. Trap: the header has room for only 1008 slots, about 1 GiB at the common 1 MiB band, and no reference says how bigger images extend the table (the unknown field at byte 32 is a candidate). A file with data past its last mapped slot is refused as unsupported rather than misread; settle this against a large real image.
- **DiskCopy 4.2**: 84-byte header, then data. Common for classic HFS floppies.

## HFS+ and HFSX

Everything is big-endian. Primary source: Apple Technical Note TN1150.

**Volume header.** 512 bytes at byte 1024 of the volume; a copy sits 1024 bytes before the end. Signature `H+` (version 4) or `HX` (version 5, HFSX). It holds the block size, block counts, the write count, and five fork descriptors for the special files: allocation, extents overflow, catalog, attributes, startup.

**Wrapped volumes.** An HFS master directory block (`BD`) whose embedded-signature field is `H+` wraps an HFS+ volume. The embedded extent gives its offset; all HFS+ offsets are then relative to that.

**Forks.** A fork descriptor has a logical size, a block count and the first eight extents (start block, block count). Further extents are in the extents overflow B-tree, keyed by fork type (0 data, 0xFF resource), file ID and the fork-relative start block. Do not assume eight is enough, including for the catalog file itself.

**B-trees.** Node 0 is the header node; it gives node size, root, first and last leaf, depth and counts. Every node begins with a 14-byte descriptor (forward link, backward link, kind, height, record count). Record offsets are 16-bit values stored backwards from the end of the node. Validate, per node: the kind, the record count against the node size, every offset inside the node and in ascending order, key length inside the record. Bound sibling walks by the total node count.

**Catalog.** Key: parent ID plus a name of up to 255 UTF-16 units. Record kinds: folder, file, folder thread, file thread. A thread record is keyed by (own ID, empty name) and gives the parent and name, which is how an ID becomes a path. All children of a folder are contiguous in key order: seek to (folder ID, empty name) and walk leaves until the parent ID changes. That is the directory listing, already sorted.

**Name comparison.** HFS+ compares names case-insensitively with Apple's own folding table from TN1150 (some code points are ignored entirely). It is not Unicode case folding and not ASCII lowercase. HFSX stores a compare type in the catalog header: `0xCF` case folding, `0xBC` binary. Get this wrong and lookups miss files that exist.

**Unicode.** Names are stored decomposed, using Apple's variant of canonical decomposition (certain ranges are excluded). Compose for display. Decompose the same way for lookup. A stored name may contain `/`; it never contains `:`.

- Excluded from decomposition: U+2000–U+2FFF and U+F900–U+FAFF (TN1150), and every character outside the BMP (the macOS converter decomposes 16-bit code units only; surrogate pairs pass through). The non-BMP rule is from the kernel's behaviour, not TN1150; confirm it in the conformance pass.
- TN1150's printed decomposition table is the Unicode 2.1-era one. Mac OS X 10.3 and later use Unicode 3.2, which is what we implement. They differ on about 140 entries, mostly Greek accents. A name written by Mac OS 8.1–10.2 containing one of those may be stored in the older form: it lists correctly, but looking it up from a typed name can miss. A fallback lookup is a possible follow-up if real disks show it.
- The full TN1150 case-folding table is not in the technote's text; it is in `FastUnicodeCompare.c` inside the technote's `tn1150.1.hqx` download (BinHex around StuffIt/Arsenic). `tools/hfs_unicode/README.md` records how it was extracted and the hashes.

**Dates.** Unsigned seconds since 1904-01-01. Stored in GMT, except the volume creation date, which is local time.

**Reserved IDs.** 1 is the parent of the root, 2 is the root folder, 3 to 8 are the special files, user IDs start at 16.

**Links.** A file hard link is a small file with type `hlnk` and creator `hfs+`; its link reference names the real file `iNode<number>` inside a hidden root folder whose name is four NUL characters followed by `HFS+ Private Data`. Directory hard links (Time Machine) have type `fdrp`, creator `MACS`, and point to `dir_<number>` inside `.HFS+ Private Directory Data` followed by a carriage return. Symbolic links are files whose mode says symlink; the data fork holds the target path. Hide the private folders; resolve the links.

**Extended attributes.** The attributes B-tree is keyed by file ID, attribute name and start block. Values are inline, or a fork descriptor, or extra extents.

**Compression.** A file with the compressed flag has a `com.apple.decmpfs` attribute and an empty data fork. The attribute header (magic, kind, uncompressed size) is little-endian. Depending on the kind, the payload is inside the attribute or in the resource fork in 64 KB chunks, using zlib, LZVN or LZFSE, with uncompressed variants. Report the real size, decompress on read, and never show the resource fork of a compressed file as a user fork. Confirm chunk-table layouts against a reference implementation.

**Journal.** If the volume is journaled and was not unmounted cleanly, the journal may hold newer copies of metadata blocks. Never write the replay to disk. Build an in-memory overlay (block number to journal copy) and have the block layer consult it. A clean journal needs nothing.

Checked against Apple's `hfs` sources (`core/hfs_journal.c` `replay_journal` and `journal_open`, `core/hfs_vfsutils.c` `hfs_early_journal_init`, `core/hfs_vfsops.c`). The traps:

- **When to replay.** macOS replays only when the journaled bit is set *and* `lastMountedVersion` is `'HFSJ'`. Any other last mounter (Linux, fsck, an old OS) did not journal its writes, so the journal is stale and must be ignored. The unmounted bit does not decide it: a non-empty journal (`start != end`) does.
- **Journal info block** (big-endian, at allocation block `journalInfoBlock`): flags (`1` in this volume, `2` on another device, `4` needs initialization, meaning nothing valid), `offset` u64 @36 and `size` u64 @44. `offset` counts from the start of the HFS+ volume (the embedded one when wrapped).
- **Byte order.** The journal header and block lists are in the *writer's* byte order, little-endian on Intel and Apple silicon Macs. The magic (`JNLx` 0x4A4E4C78, old `JHDR`) tells which.
- **Header** (48 bytes): magic, endian, start i64, end i64, size i64, blhdr_size, checksum, jhdr_size, sequence_num. The checksum covers the first 44 raw bytes with the checksum zeroed. xnu only logs a mismatch. Sanity bounds from `journal_open`: size 256 KiB to 1 GiB; start and end in (0, size], 512-aligned.
- **Ring.** Data lives in [jhdr_size, size); a position reaching `size` continues at `jhdr_size`, and reads and blocks may span the wrap.
- **Block list header:** max_blocks u16, num_blocks u16, bytes_used i32, checksum (first 32 raw bytes, checksum zeroed), flags (`1` check block checksums, `2` first block list of a transaction). This is followed by 16-byte `block_info` entries (bnum i64, bsize i32, checksum u32). Entry 0 is not a block: its last field is the transaction sequence number. Block data follows the header in entry order; `bytes_used` = blhdr_size + Σ bsize.
- **Block addresses** are `bnum × jhdr_size` bytes from the start of the *partition* (the wrapper start, not the embedded volume). `bnum == -1` is a killed block: skip it, but its bytes still occupy the ring.
- **Damage.** A bad block list ends the replay; xnu keeps the complete transactions before the last `first header` list and drops the rest. Transactions past `end` that continue the sequence numbers and pass their checksums are replayed too: they were written, but the header update was not.
- macOS refuses a read-only mount of a dirty journal. Replaying it in memory, as Disk Atlas does, shows more than macOS would without writing.

## HFS (classic)

Big-endian. Primary source: Inside Macintosh: Files.

- Master directory block at byte 1024, signature `BD`. Allocation blocks are numbered from a start sector given in the MDB; block size is in the MDB; counts are 16-bit.
- Catalog and extents overflow B-trees with 512-byte nodes. Three extents per fork before overflow, 16-bit fields.
- Catalog key: parent ID plus a Pascal string of up to 31 bytes in a Mac script encoding, usually Mac OS Roman. Comparison is case-insensitive with its own ordering table.
- Dates are local time, same 1904 epoch.
- The B-tree node format matches HFS+ closely enough to share the node reader, with different key and record parsers.

## APFS

Little-endian. Primary source: Apple File System Reference.

- Every object starts with a 32-byte header: Fletcher-64 checksum, object ID, transaction ID, type, subtype. Verify the checksum before trusting a block.
- Block 0 holds a container superblock (`NXSB`), which may be stale. Walk the checkpoint descriptor area and use the valid superblock with the highest transaction ID.
- The container object map (a B-tree keyed by object ID and transaction ID) turns virtual object IDs into block addresses: take the entry with the greatest transaction ID not above the one you are reading at.
- Each volume has a superblock (`APSB`), its own object map, and a file-system tree. Tree keys combine an object ID with a record type in the top four bits: inode, extended attribute, sibling link, data stream, file extent, directory entry and others. Directory entry keys carry a hash of the normalized name on case-insensitive volumes.
- Timestamps are nanoseconds since 1970 UTC. Compression uses the same `com.apple.decmpfs` scheme as HFS+.
- Plan for snapshots, multiple volumes sharing free space, sealed system volumes, and Fusion containers as separate steps. Encryption: software-encrypted volumes unlock through the keybag with the user's password; hardware-bound volumes do not.

## Copying files out

The host filesystem cannot hold everything. When copying to the host: sanitize names that are illegal there (reserved characters and names on Windows, trailing dots and spaces), keep resource forks and Finder metadata in an AppleDouble `._name` sidecar when the user asks for forks, set modification times, and never overwrite silently.

## Test material

- Unit tests build the smallest byte fixtures in code: one node, one record, one malformed variant each.
- Small real images (a few hundred KB, compressed) live in `crates/lib/tests/fixtures/`. Make them with `mkfs.hfsplus` on Linux or WSL, `hdiutil` on macOS, `hformat` from hfsutils for classic HFS.
- Large or personal images are never committed. Tests that need them read a path from an environment variable and skip when it is unset.
- Every parser that takes raw bytes gets a `cargo fuzz` target.

## Sources and licensing

Primary: Apple TN1150 (HFS+), Inside Macintosh: Files (HFS), Apple File System Reference (APFS), the UEFI specification (GPT). Useful cross-checks: the libyal format documents (libfshfs, libfsapfs), hfsutils, The Sleuth Kit.

Read reference implementations to understand a format; do not copy from them. In particular the Linux kernel drivers are GPL-2.0-only and Apple's own sources are APSL, and neither can be copied into this GPL-3.0-or-later project.
