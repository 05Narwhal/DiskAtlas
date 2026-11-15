# M7. APFS, read-only

Goal: an APFS container shows its volumes in the sidebar; each lists, previews, searches and copies like an HFS+ volume; snapshots can be browsed; a software-encrypted volume opens with its password.

Primary source: Apple's *Apple File System Reference* (the PDF, 2020 edition or later). Cross-checks: the libyal `libfsapfs` format document, and the behaviour (not the code: it is GPL-2) of `apfs-fuse`. The design (what is cached, how lookups go, the bounds, the API) does not depend on an individual offset.

**Verification (Fable, 2026-11-12; this was task 1).** Every offset in §3.1–§3.7, §6, §9.2 and §10 was checked against the packed struct definitions in `apfs_raw.h` of the `linux-apfs-rw` driver, by computing each field's offset mechanically, and every flag and type constant against the same file. **All offsets agree.** The structs covered: object header, container superblock (through `nx_keylocker` at 1296), checkpoint map and mapping, object map header, key and value, B-tree node header and tree info, volume superblock (through `apfs_fext_tree_type` at 1040), inode value, extended-field blob, data stream, hashed and plain directory record keys and value, xattr value and stream, file extent value, sealed file-extent key and value, sibling value, snapshot metadata value, space manager and its device record, chunk info, chunk-info block, address block, keybag locker and entry. Facts were read; nothing was copied. The check turned up five things the tables did not say; they are marked **(added)** below:

1. Trees on sealed volumes have nodes with a zeroed object header and no checksum (§3.5, "No-header trees"). Without this rule every sealed system volume fails at its first node.
2. Index-node values in hashed trees are a child id followed by a hash whose length depends on the hash function (32 bytes for SHA-256, up to 64). Read the first 8 bytes and never assume the value's length (§3.5).
3. A file extent can be flagged preallocated; it reads as zeros (§3.7, §5.3).
4. Three newer incompatible-feature bits exist (§3.6).
5. What the key blobs hold, from `apfs-fuse`'s behaviour (§9.3).

Still to check against the PDF and a real container by the task that writes each parser: the order of name comparison inside one object's xattr and directory records (§3.7; only `seek` to the first record of a type depends on it, and that part is certain), items 1 and 3 above on a sealed system volume image, and §9.3 on a real keybag.

## 1. Decisions

1. **Two objects, not one.** A `Container` (one per partition: superblock, container object map, block cache, optional keys) and a `Volume` (`FileSystem` impl) that holds an `Arc<Container>`. Volumes of one container share everything below them.
2. **Mounting can return several volumes.** `probe::mount` returns a `Mount` enum (§7). This is the one engine API change the app must follow; it is written so no APFS word appears in the app.
3. **Checksums are verified once per block.** A verified metadata block is kept in a small decoded-node cache (the chunk cache below holds raw bytes and cannot remember that a block was checked).
4. **Name hashes are not computed.** Listing a folder walks all its directory records and never needs the hash. `lookup(dir, name)` walks the folder and compares names. This removes the need for full Unicode NFD tables and case folding to *exactly* Apple's version, the classic source of "file exists but cannot be found" bugs. It costs time only on typed paths.
5. **Folder listings get sizes and dates with a moving cursor,** not a fresh descent per entry: a directory's children usually have neighbouring inode numbers, so their inode records share leaves (§5.4).
6. **The disk strip uses chunk free counts,** not the bitmaps: the space manager already stores "blocks free" per 32,768-block chunk, which is exactly a density map and needs a few dozen reads for a terabyte.
7. **Sealed system volumes (macOS 11+) are in scope**: their file extents live in a separate tree. Without it every file on a modern system volume reads empty.
8. **Out of scope, detected and named:** Fusion containers; hardware-bound encryption (T2, Apple silicon internal disks); per-file encryption (iOS); a container whose checkpoint descriptor area is not contiguous; volumes mid-way through encryption rolling. Each is `Error::Unsupported` with those words.
9. **Crypto dependencies:** `aes` 0.8, `sha2` 0.10, `hmac` 0.12, `pbkdf2` 0.12 (RustCrypto; MIT or Apache-2.0; all in the lock file through GPUI). XTS mode and RFC 3394 key unwrap are about 120 lines on top of `aes` and are written in-house with the standards' test vectors, instead of two more crates. Julian approves (README decision 9). They are declared only at the unlock task.

## 2. Module layout (`fs/apfs/`)

| File | Holds | Owner |
|---|---|---|
| `bytes.rs` | checked little-endian reads (or widen `hfsplus/bytes.rs` into `fs/bytes.rs`: H moves it, both use it) | H |
| `object.rs` | object header, type constants, `fletcher64`, `verify(block, expect) -> Result<Header>` | S |
| `container.rs` | `NxSuperblock::parse`, checkpoint selection, checkpoint map, `Container` (block reads, node cache, ephemeral lookup) | O |
| `omap.rs` | `Omap`: header, lookup at a transaction, resolution cache | O |
| `btree.rs` | node view, root info, `Tree`, `Cursor` (seek and in-order iteration without sibling links) | O |
| `records.rs` | file-system key and value parsers, extended fields, the key comparator | O |
| `volume.rs` | `ApfsSuperblock::parse`, `Volume`, the `FileSystem` impl, `ApfsDir`, file sources | O |
| `snapshot.rs` | snapshot metadata tree, `snapshots`, `open_snapshot` | O |
| `spaceman.rs` | `allocation_map` from chunk info | S |
| `crypto.rs` (late) | keybags, key blobs, unlock, XTS, key unwrap, `DecryptingSource` | Fable designs the brief from §9, O builds |
| `test_container.rs` | an in-memory container builder for tests and fuzz seeds | S |
| `mod.rs` | `mount(source) -> Result<Vec<ContainerVolume>>` | O |

The decmpfs decoder is shared: H moves `hfsplus/compression.rs` to `fs/decmpfs.rs` and leaves a re-export in `hfsplus` (its `open` already takes the resource fork as a closure, so nothing else changes).

## 3. On-disk structures

Little-endian throughout. A `paddr` is a block number; byte offset = `paddr × block_size`, from the start of the container (the partition).

### 3.1 Object header (32 bytes, at the start of every object unless "no header")

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | `o_cksum`: Fletcher-64 of the rest of the block |
| 8 | 8 | `o_oid` |
| 16 | 8 | `o_xid`: transaction that wrote it |
| 24 | 4 | `o_type`: low 16 bits type, high bits storage flags |
| 28 | 4 | `o_subtype` |

Storage flags: `0x8000_0000` ephemeral, `0x4000_0000` physical, neither = virtual; `0x2000_0000` no header; `0x1000_0000` encrypted.
Types used: `1` container superblock, `2` B-tree root, `3` B-tree node, `5` space manager, `7` chunk-info block, `0xB` object map, `0xC` checkpoint map, `0xD` volume superblock, `0xE` file-system tree, `0xF` extent-reference tree, `0x10` snapshot metadata tree, `0x1F` file-extent tree; keybags `'keys'` = `0x6B65_7973`, `'recs'` = `0x7265_6373`.

**Fletcher-64** over the block from byte 8, as little-endian u32 words: `sum1 = (sum1 + word) mod (2^32 − 1)`, `sum2 = (sum2 + sum1) mod (2^32 − 1)`; then `c1 = (2^32 − 1) − ((sum1 + sum2) mod (2^32 − 1))`, `c2 = (2^32 − 1) − ((sum1 + c1) mod (2^32 − 1))`; checksum = `c2 << 32 | c1`. Accumulate in `u64` and reduce every 1024 words (prove in a comment that neither sum can overflow in that span), not per word. A checksum of all zero bytes is invalid. Test vector: build a block, compute, and check that verifying with the stored value passes and any flipped bit fails; also check one real block from a fixture image.

`verify(block, expect)` checks, in order: checksum; `o_type & 0xFFFF` is one of the expected types; when the caller knows it, `o_oid`. A failure is `Corrupt` with `Location::Block(paddr)`. The one exception is a block the caller knows to be stored without a header (§3.5, "No-header trees"): `verify` is not called for it at all.

### 3.2 Container superblock (`NXSB`)

| Offset | Size | Field |
|---|---|---|
| 32 | 4 | magic `NXSB` |
| 36 | 4 | `nx_block_size` |
| 40 | 8 | `nx_block_count` |
| 48 / 56 / 64 | 8 each | features, read-only-compatible features, incompatible features |
| 72 | 16 | `nx_uuid` |
| 88 / 96 | 8 each | next oid, next xid |
| 104 | 4 | `nx_xp_desc_blocks` (high bit: area is not contiguous) |
| 108 | 4 | `nx_xp_data_blocks` |
| 112 | 8 | `nx_xp_desc_base` (paddr) |
| 120 | 8 | `nx_xp_data_base` |
| 128–151 | 4 each | desc next, data next, desc index, desc len, data index, data len |
| 152 | 8 | `nx_spaceman_oid` (ephemeral) |
| 160 | 8 | `nx_omap_oid` (physical: it is a paddr) |
| 168 | 8 | reaper oid |
| 180 | 4 | `nx_max_file_systems` (≤ 100) |
| 184 | 800 | `nx_fs_oid[100]` (virtual oids; 0 = unused) |
| 1264 | 8 | `nx_flags` |
| 1296 | 16 | `nx_keylocker`: paddr u64, block count u64 |

Incompatible features: `0x1` version 1 (pre-release: unsupported), `0x2` version 2 (required), `0x100` Fusion (unsupported).

Validation: block size a power of two from 4096 to 65536; `block_count × block_size` computed checked and clamped to the source length (a truncated image); `xp_desc_blocks & 0x7FFF_FFFF` from 1 to `MAX_XP_DESC = 65_536`; the descriptor area inside the container; `max_file_systems <= 100`.

### 3.3 Checkpoints — choosing the superblock

Block 0 is a copy that may be stale. The checkpoint descriptor area (`xp_desc_blocks` blocks from `xp_desc_base`) is a ring of checkpoint-map blocks and superblock copies.

1. Read block 0 as 4096 bytes; verify; take `block_size`; re-read block 0 at that size if larger.
2. Read every block of the descriptor area (one `read_bulk` when it is under 16 MiB, else block by block). Keep each block that verifies as a container superblock with the same `nx_uuid` and block size as block 0. Sort by `o_xid`, highest first.
3. For each candidate in that order: its checkpoint is `nx_xp_desc_len` blocks starting at `nx_xp_desc_index` within the ring (wrapping); all but the last are checkpoint maps, the last is the superblock itself. Accept the candidate when every map block verifies (type `0xC`) and the container object map at `nx_omap_oid` verifies (type `0xB`). The first accepted candidate is the container's state. None accepted: fall back to block 0 under the same test; else fail the mount.
4. Keep the accepted candidate's checkpoint-map entries in memory (bounded: at most `xp_desc_len × (block_size / 40)` entries, and `MAX_EPHEMERAL = 65_536`).

Checkpoint map block: `cpm_flags` u32 @32, `cpm_count` u32 @36, then 40-byte entries from 40: type u32, subtype u32, size u32, pad u32, fs oid u64, oid u64, paddr u64. This is how an *ephemeral* oid (the space manager) becomes an address.

### 3.4 Object map

Header (after the object header): flags u32 @32, snapshot count u32 @36, tree type u32 @40, snapshot tree type u32 @44, `om_tree_oid` u64 @48 (physical), snapshot tree oid u64 @56, most recent snapshot xid u64 @64.

The tree: fixed-size keys `(oid u64, xid u64)` and values `(flags u32, size u32, paddr u64)`. Ordered by oid, then xid. Value flags: `0x1` deleted, `0x4` encrypted, `0x8` no header.

`Omap::lookup(oid, max_xid) -> Result<Option<Mapping { paddr, flags, xid }>>`: seek the last key `<= (oid, max_xid)`; a hit must have the same oid; a deleted mapping is `None`. Resolutions are cached in a sharded `HashMap<(u64, u64), Mapping>` capped at `OMAP_CACHE = 65_536` entries per object map (clear the shard when full; no LRU needed).

### 3.5 B-tree nodes

| Offset | Size | Field |
|---|---|---|
| 32 | 2 | `btn_flags`: `0x1` root, `0x2` leaf, `0x4` fixed-size keys and values, `0x8` hashed, `0x10` no header |
| 34 | 2 | `btn_level` (0 = leaf) |
| 36 | 4 | `btn_nkeys` |
| 40 | 4 | table space: offset u16, length u16 (offset is from byte 56) |
| 44 | 4 | free space |
| 48 | 4 | key free list |
| 52 | 4 | value free list |
| 56 | … | table of contents, then the key area, free space, then values growing back from the end |

- Table of contents entry: fixed trees 4 bytes `(key offset u16, value offset u16)`; variable trees 8 bytes `(key offset u16, key length u16, value offset u16, value length u16)`.
- Key offsets count forward from the start of the key area, which is `56 + table.offset + table.length`.
- Value offsets count **backward** from the end of the value area: the end of the block, or the end minus 40 in a root node, where the last 40 bytes are the tree info. A value starts at `end − offset`.
- In a fixed tree a value offset of `0xFFFF` means "no value" (a ghost); skip the entry.
- In a non-leaf node the value's first 8 bytes are the child's object id. **(added)** In a hashed tree (sealed volumes) a hash of the child follows, 32 bytes for SHA-256 and up to 64 for other functions (the reference and real volumes disagree on the stored length). Read the 8 bytes, require the value to be at least 8 long, ignore the rest. In a *fixed-size* tree the tree info's value size is the leaf value size; an index value is 8 bytes there.
- Tree info (root only, last 40 bytes): flags u32, node size u32, key size u32, value size u32, longest key u32, longest value u32, key count u64, node count u64. Info flags: `0x10` physical children (child ids are paddrs), `0x8` ephemeral (unsupported outside the space manager's free queue, which is not read), else virtual (resolve each child through the tree's object map at the tree's transaction).

**No-header trees (added).** A tree can be stored without object headers: the first 32 bytes of every node are zero, so there is no checksum, no object id and no type to check. Sealed volumes (incompatible feature `0x20`) store their file-system tree this way, and the reader must not assume which other trees do; it follows the flags. How the reader knows, without trusting the node itself:

- Each tree is named in the volume superblock by an oid and a *type* field (`apfs_root_tree_type` @116, `apfs_fext_tree_type` @1040, and so on). The type field carries the storage flags of §3.1. When it has `0x2000_0000` (no header), every node of that tree is a no-header node. (Confirmed constant.)
- For a virtual node, the object map value's flag `0x8` (no header) says the same for that one block. (Confirmed constant.) Either signal is enough.
- Inside the tree the same fact is repeated by tree info flag `0x100` and node flag `0x10`, and hashing by info flag `0x80` and node flag `0x8`. These four values are from memory of Apple's reference and were *not* in the cross-check source: confirm them in the PDF, and do not make the reader depend on them. They are for a consistency check only (a node of a no-header tree whose own flag disagrees is logged at debug, not failed).
- `Tree` gets a field `headers: bool`, set from the type field when the tree is opened. With `headers == false`, `Container::node` skips `verify` and requires instead that the first 32 bytes are all zero (a node of such a tree that *has* a header is damage); everything else in `NodeView::parse` applies unchanged, and it is then the only line of defence, so none of its checks may be relaxed.
- What is lost: on a sealed volume a flipped bit in a node is not detected by a checksum. Apple's protection there is the hash tree (the `0x8` hashed flag), which this reader does not verify (out of scope: it would need the seal's root hash chain). Say so in `filesystems.md`.

**Validation, per node, before any record is handed out** (`NodeView::parse`): the block verifies with type 2 or 3 (or is a no-header node, above); `nkeys <= (block_size − 56) / entry_size`; the table lies inside the block before the value area; for every entry used, key and value ranges lie inside their areas (checked subtraction for the backward value offsets); `level <= MAX_LEVEL = 16`. Fixed trees take key and value sizes from the tree info and they must be the expected sizes for the tree's kind (object map: 16 and 16).

**`Tree` and `Cursor`.**

```rust
pub(crate) struct Tree { container: Arc<Container>, root: u64 /*paddr*/, children: Children, info: TreeInfo }
pub(crate) enum Children { Physical, Virtual { omap: Arc<Omap>, xid: u64 } }

impl Tree {
    /// Positions on the first record whose key is not less than the target.
    pub fn seek(&self, cmp: &mut dyn FnMut(&[u8]) -> Result<Ordering>) -> Result<Cursor>;
    /// The last record whose key is not greater than the target (for the object map).
    pub fn seek_le(&self, cmp: ...) -> Result<Option<(Vec<u8>, Vec<u8>)>>;
}
impl Cursor {
    /// The current record's key and value, then advances. `None` at the end.
    pub fn next(&mut self) -> Result<Option<(&[u8], &[u8])>>;
    /// Repositions, reusing the current leaf when the target lies inside it.
    pub fn seek(&mut self, cmp: ...) -> Result<()>;
}
```

There are no sibling links: the cursor holds the path from the root as `(Arc<Node>, index)` pairs (at most 16) and advances by climbing to the first ancestor with a next child and descending leftmost. Descent uses binary search in a node (keys in a node are sorted), choosing the last child whose key is `<=` the target. Bounds: a child's level must be exactly its parent's minus one; a child whose paddr is outside the container fails; `Cursor::next` across the whole tree visits at most `info.node_count + 1` nodes, enforced with a counter (a tree that references itself cannot loop forever, because levels strictly decrease, so the counter is a second line).

`Cursor::seek`'s leaf reuse is decision 5: before descending, compare the target with the first and last key of the leaf held; inside → binary search there.

**Node cache** (`Container`): `(paddr) → Arc<Node>` of verified nodes, sharded, CLOCK, `NODE_CACHE = 4096` nodes. A `Node` is the block's bytes plus the parsed header fields. Decrypted volumes key the cache by `(volume index, paddr)`.

### 3.6 Volume superblock (`APSB`)

| Offset | Size | Field |
|---|---|---|
| 32 | 4 | magic `APSB` |
| 36 | 4 | index in the container |
| 40 / 48 / 56 | 8 each | features, read-only-compatible, incompatible |
| 64 | 8 | unmount time |
| 88 | 8 | `apfs_fs_alloc_count` (blocks used) |
| 96 | 20 | metadata crypto state |
| 116 / 120 / 124 | 4 each | root tree type, extent-ref tree type, snapshot-metadata tree type |
| 128 | 8 | `apfs_omap_oid` (physical) |
| 136 | 8 | `apfs_root_tree_oid` (virtual) |
| 144 | 8 | extent-ref tree oid |
| 152 | 8 | `apfs_snap_meta_tree_oid` (physical) |
| 184 / 192 / 200 | 8 each | file, folder and symlink counts |
| 216 | 8 | snapshot count |
| 240 | 16 | `apfs_vol_uuid` |
| 256 | 8 | last modified time |
| 264 | 8 | `apfs_fs_flags`: `0x1` unencrypted, `0x8` one key for the volume |
| 704 | 256 | volume name, UTF-8, NUL-terminated |
| 964 | 2 | role: `0x1` system, `0x2` user, `0x4` recovery, `0x8` VM, `0x10` preboot, `0x40` data |
| 1032 | 8 | `apfs_fext_tree_oid` (physical; sealed volumes) |
| 1040 | 4 | fext tree type |

Incompatible features: `0x1` case-insensitive, `0x2` dataless snapshots, `0x4` encryption rolled (unsupported), `0x8` normalization-insensitive, `0x10` incomplete restore (mount anyway), `0x20` sealed. **(added)** Three newer bits that Apple's reference does not name (the names are the Linux driver's guesses): `0x40` per-file keys, `0x80` file extents may carry the preallocated flag, `0x100` a secondary file-system root (`apfs_sec_root_tree_oid` @1096, type @1104). Policy:

| Bit | What the reader does |
|---|---|
| `0x1`, `0x2`, `0x8`, `0x10`, `0x20`, `0x80` | Mount. |
| `0x4` | `Unsupported("this volume is part-way through changing its encryption")`. |
| `0x40` | Mount only when `apfs_fs_flags` has `0x1` (unencrypted). Otherwise it is per-file encryption: `Unsupported`, with the sentence for hardware-bound volumes. |
| `0x100` | Mount and read the main tree. Entries that live only in the secondary tree are not shown; record that in `filesystems.md`. If a real disk shows missing files because of it, that is a stop-and-ask. |
| Any other bit | `Unsupported("this APFS volume uses a feature newer than this reader (incompatible feature 0x…)")`. A bit this reader has never heard of may change how records are laid out; showing wrong contents is worse than showing none. |

The same rule for the container: `nx_incompatible_features` must be exactly `0x2`; `0x1` and `0x100` (Fusion) are `Unsupported` by name, any other bit by number.

A volume is found from `nx_fs_oid[i]` (non-zero): container object map lookup at the container's xid → paddr → verify as type `0xD` with that oid.

### 3.7 File-system records

Every key starts with a u64: object id in the low 60 bits, record type in the top 4.

**Key order (the comparator, `records::compare`)**: object id, then record type, then by type: extended attribute and non-hashed directory record: name bytes (compare as the reference specifies; **verify** whether length is compared first); hashed directory record: the 32-bit `name_len_and_hash` field, then name bytes; file extent: logical address; others: equal. Never compare the raw u64: the type is in the high bits.

| Type | Key after the header | Value |
|---|---|---|
| 3 inode | — | below |
| 4 xattr | name length u16, name (NUL-terminated) | flags u16, data length u16, data. Flags: `0x1` data is a stream descriptor (xattr object id u64, then a 40-byte data-stream record), `0x2` data is embedded, `0x4` file-system owned |
| 5 sibling link | sibling id u64 | parent id u64, name length u16, name |
| 6 data-stream id | — | reference count u32 |
| 8 file extent | logical address u64 | length-and-flags u64 (length in bytes, low 56 bits; flags in the top 8: **(added)** `0x02` preallocated, meaning space reserved and never written, which reads as zeros), physical block u64 (0 = hole), crypto id u64 |
| 9 directory record | hashed volumes: `name_len_and_hash` u32 (length low 10 bits, including the NUL), name. Others: name length u16, name | file id u64, date added u64, flags u16 (low 4 bits: 4 folder, 8 file, 10 symlink), extended fields |
| 12 sibling map | — | file id u64 |

Hashed keys are used when the volume is case-insensitive or normalization-insensitive (incompatible feature bits `0x1` or `0x8`), which is nearly every macOS volume.

**Inode value:**

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | parent id |
| 8 | 8 | private id (the object id its file extents are keyed by) |
| 16 / 24 / 32 / 40 | 8 each | created, modified, changed, accessed (ns since 1970) |
| 48 | 8 | internal flags (`0x0004_0000`: has uncompressed size) |
| 56 | 4 | children (folder) or link count (file) |
| 60 | 4 | default protection class |
| 64 | 4 | write generation |
| 68 | 4 | BSD flags (`0x20` compressed, `0x8000` hidden) |
| 72 / 76 | 4 each | owner, group |
| 80 | 2 | mode |
| 84 | 8 | uncompressed size |
| 92 | … | extended fields |

**Extended fields:** count u16, used bytes u16, then `count` 4-byte descriptors (type u8, flags u8, size u16), then the data, each field padded to 8 bytes. Inode field types: `4` name (NUL-terminated UTF-8), `7` Finder info (32 bytes), `8` data stream (size u64, allocated u64, default crypto id u64, bytes written u64, bytes read u64), `13` sparse bytes, `14` device number. Parser bounds: `count <= 64`; descriptors and data inside the value; a field that runs past the end stops the parse and keeps the fields before it.

Reserved inode numbers: 1 root's parent, 2 root folder, 3 private directory, 6 snapshot directory; user inodes start at 16.

## 4. `Container` (`container.rs`)

```rust
pub(crate) struct Container {
    source: Arc<dyn BlockSource>,
    pub block_size: u32, pub block_count: u64,
    pub uuid: [u8; 16], pub xid: u64,
    superblock: NxSuperblock,
    omap: Omap,
    ephemeral: Vec<CheckpointMapping>,
    nodes: NodeCache,
}
impl Container {
    pub fn open(source: Arc<dyn BlockSource>) -> Result<Arc<Self>>;       // §3.3
    pub fn read_block(&self, paddr: u64, buf: &mut [u8]) -> Result<()>;   // range-checked against block_count
    pub fn node(&self, paddr: u64, decrypt: Option<&VolumeKey>) -> Result<Arc<Node>>;
    pub fn ephemeral(&self, oid: u64) -> Option<&CheckpointMapping>;
    pub fn volumes(self: &Arc<Self>) -> Vec<ContainerVolume>;            // §7
}
```

## 5. `Volume` (`volume.rs`)

```rust
pub struct Volume {
    container: Arc<Container>,
    sb: ApfsSuperblock,
    omap: Arc<Omap>,
    xid: u64,                      // the volume superblock's, or a snapshot's
    fs_tree: Tree,
    fext_tree: Option<Tree>,       // sealed volumes
    key: Option<VolumeKey>,        // unlocked encrypted volumes
    hashed: bool, case_insensitive: bool,
    info: VolumeInfo, identity: VolumeIdentity,
}
```

### 5.1 Object visit

The primitive everything uses: `visit_object(cursor, id, from_type, f)`: `cursor.seek((id, from_type, lowest))`, then `next()` while the key's object id is `id`, handing each `(type, key, value)` to `f`, which returns `Continue` or `Break`. Records of one object are contiguous and ordered by type: inode (3), xattrs (4), sibling links (5), stream id (6), crypto (7), file extents (8), directory records (9).

`InodeFacts` (kind from `mode`, sizes, dates, flags, link target, Finder info, owner, private id, stream size, compressed flag and uncompressed size) comes from the inode record plus the xattr records that follow it: `com.apple.ResourceFork` (its stream size is the resource size), `com.apple.decmpfs` (when the inode lacks an uncompressed size: parse the 16-byte header from embedded data), `com.apple.fs.symlink` (the target, embedded).

- **Size of a compressed file:** the inode's uncompressed size when internal flag `0x40000` is set, else the decmpfs header's. `resource_size` of a compressed file is `Some(0)`, as on HFS+.
- **Hidden:** BSD flag `0x8000` or a leading dot.
- **Kind:** from the mode's type bits (`0o040000` folder, `0o100000` file, `0o120000` symlink, else other).

### 5.2 Trait mapping

- `info`: the volume name; format "APFS"; `total_bytes` = container size; `free_bytes` from the space manager's free count when it was readable at mount, else `None`; `resource_forks: true`; `journal: None`.
- `root`: `NodeId(2)`. `NodeId` is the inode number.
- `metadata(node)`: one object visit from type 3 through type 4; `attributes` counts xattrs except the three internal names above.
- `read_dir(dir)`: `ApfsDir` holds two cursors over the same tree: `entries` positioned at `(dir, 9)` and `inodes`. `next_batch`: for each directory record: name (strip the NUL), child id, kind from the record's flags; then `inodes` does an object visit of the child for sizes, dates and flags (decision 5). A child whose inode cannot be read is still listed, with no size or date. First check `dir` is a folder (its inode's mode) for `NotADirectory`/`NotFound`.
  - Budget check for the lead: list a 100,000-entry folder from the test builder and from a real image; the 500 ms budget is expected to hold when children are clustered and may not on an old, churned volume. Report the figure either way.
- `lookup(dir, name)`: walk the folder's records; exact byte match wins at once; otherwise, on an insensitive volume, match when the two names are equal after Apple decomposition (`hfsplus::unicode::decompose`, which is BMP-only: good enough here) and, when case-insensitive, `char::to_lowercase`.
- `open(node, Data)`: compressed → `decmpfs::open` with the `com.apple.decmpfs` xattr value and a closure opening the `com.apple.ResourceFork` xattr stream. Otherwise an `ExtentSource` over the inode's private id and stream size. `open(node, Resource)`: the `com.apple.ResourceFork` xattr (embedded → `MemSource`; stream → `ExtentSource` over the xattr's object id), `NoSuchStream` when absent or the file is compressed. Symlink: a `MemSource` of the target.
- `extents`: the same extent list as byte runs (`paddr × block_size`), holes left out, adjacent runs merged.
- `xattrs`, `read_xattr`: xattr records of the object, internal names left out; embedded values directly, stream values read whole up to `max`.
- `scan`: one in-order pass over the whole file-system tree. For each inode record (type 3) with id ≥ 16: name from extended field 4, parent from the value, facts as in a listing except that the resource fork size and a header-only decmpfs size come from the xattr records that follow in the same pass (hold the inode's entry until the object id changes, then visit). Inodes without a name field are skipped and counted. Additional names of a hard-linked file (sibling links) are not reported: the file is found under its primary name. Entries whose parent is the private directory (3) arrive with a parent that is never reported and are dropped by the index builder.
- `identity`: `id` = the volume UUID; `change` = the volume's `o_xid` and last-modified time.
- `allocation_map`: §6.

### 5.3 `ExtentSource` (file content)

Built at `open` by walking file-extent records `(private id, 8)` in logical order into a `Vec<Run { logical, len, paddr, crypto_id }>`. On a sealed volume (`fext_tree` present) the runs come from the file-extent tree instead: fixed keys `(private id u64, logical address u64)`, values `(length-and-flags u64, physical block u64)`; seek `(private id, 0)` and walk while the id matches. If the sealed tree has nothing for the file, fall back to the file-system tree.

Bounds: lengths non-zero and multiples of the block size; logical addresses ascending without overlap; every run inside the container; at most `MAX_EXTENTS = 4_000_000` runs (then `Unsupported`). `len()` is the stream's size from the data-stream field, not the sum of runs. `read_at`: binary search for the run; a hole (`paddr == 0`), a preallocated run (flag `0x02`, §3.7) or a gap between runs reads as zeros, and none of the three is ever decrypted; past the last run but inside `len()` is `Corrupt`. With a key: §9.4.

## 6. `spaceman.rs` — the disk strip

The space manager is an ephemeral object: find `nx_spaceman_oid` in the checkpoint mappings; it may span several blocks (`cpm_size`); read it whole (cap 16 MiB) and verify.

Fields (after the object header; **verify**): block size u32 @32, blocks per chunk u32 @36, chunks per chunk-info block u32 @40, chunk-info blocks per address block u32 @44; then the main device's record at 48: block count u64, chunk count u64, chunk-info-block count u32 @64, address-block count u32 @68, free count u64 @72, address offset u32 @80 (from the start of the object). At the address offset: `cib_count` paddrs of chunk-info blocks when the address-block count is 0; otherwise paddrs of address blocks, each listing chunk-info paddrs (count u32 @36, paddrs from 40).

A chunk-info block (type 7): index u32 @32, count u32 @36, then 32-byte entries from 40: xid u64, first block u64, block count u32, free count u32, bitmap paddr u64.

`allocation_map(buckets)`: for each chunk, used fraction = `(block_count − free_count) / block_count` over its block range; fold chunks into buckets with the existing density rule (any used → at least 1). `start = 0`, `len = block_count × block_size`. Bounds: chunk count ≤ `block_count / blocks_per_chunk + 1`; chunk-info blocks ≤ `MAX_CIB = 1_048_576`; `free_count <= block_count` per chunk (else treat the chunk as full). The map is container-wide: every volume of a container reports the same one, and the strip shades the container's partition once (§8).

`VolumeInfo::free_bytes` = main device free count × block size.

## 7. Engine API changes for multi-volume and locked volumes — lead

```rust
// fs/probe.rs
pub enum Mount {
    Volume(Arc<dyn FileSystem>),
    /// One source holding several volumes that share its space.
    Container(Vec<ContainerVolume>),
}
pub struct ContainerVolume {          // #[non_exhaustive]; build with ContainerVolume::new(name, role, state)
    pub name: String,
    pub role: Option<VolumeRole>,
    pub state: VolumeState,
}
/// The app orders and dims by this; `name()` gives "System", "Data", "Preboot", "Recovery", "VM".
pub enum VolumeRole { System, Data, Preboot, Recovery, Vm }   // #[non_exhaustive]
pub enum VolumeState {
    Ready(Arc<dyn FileSystem>),
    Locked(Arc<dyn LockedVolume>),
    Failed(Error),
}
pub trait LockedVolume: Send + Sync {
    /// The password hint stored on the volume, if any.
    fn hint(&self) -> Option<String>;
    /// Blocking and slow by design (key derivation). `Error::WrongPassword` when the password does not unlock it.
    fn unlock(&self, password: &str) -> Result<Arc<dyn FileSystem>>;
}
pub fn mount(source: Arc<dyn BlockSource>, content: Content) -> Result<Option<Mount>>;

// fs/mod.rs, on the trait, defaults empty / Unsupported
pub struct SnapshotInfo { pub id: u64, pub name: String, pub created: Option<Timestamp> }
fn snapshots(&self) -> Result<Vec<SnapshotInfo>>;
fn open_snapshot(&self, id: u64) -> Result<Arc<dyn FileSystem>>;
```

New `Error::WrongPassword`. Existing callers of `probe::mount` wrap their result in `Mount::Volume`.

**Done (Fable, 2026-11-12; this was task 2).** All of the above is in `fs/probe.rs`, `fs/mod.rs` and `error.rs`, and the code there is the reference. Differences from the first sketch: the role is the enum `VolumeRole` rather than a string, so the sidebar order in §8 is a `match` and not a string comparison (`0x2` user and `0x40` data both map to `Data`; a role with no variant is `None`); `Mount`, `ContainerVolume` and `VolumeState` are `#[non_exhaustive]`; `Mount::into_volume()` gives the filesystem of a single-volume mount. `LockedVolume::unlock` also documents `Unsupported` for a volume no password can open. The app's `mount_volume` matches `Mount::Volume` and, until task 8b, shows any other mount as "cannot read this yet"; no reader returns a container before task 7.

Until the unlock task lands, an encrypted volume is `VolumeState::Failed(Error::Unsupported("encrypted volume"))`.

## 8. App changes — S

- `state/sources.rs`: `mount_volume` handles `Mount::Container`: one `Volume` source per `ContainerVolume`, all with the partition's `start` and `len`, labelled with the volume name, the role in `ink3` after it ("Macintosh HD — Data" is one name; show role only when the name does not already say it). `Ready` → `Browsable`; `Failed` → `Failed(text)`; `Locked` → a new `Availability::Locked { hint }` holding the `Arc<dyn LockedVolume>` beside the source.
- **Order in the sidebar:** Data and unnamed-role volumes first, then System, then Preboot, Recovery, VM dimmed (`ink3`), since those three are rarely what anyone wants.
- **Disk strip:** volumes sharing a partition are one segment. Its label is the current volume's name when one of them is current, else the first in sidebar order; clicking it opens that one. Shading comes from whichever volume's `allocation_map` loads first (they are equal). Under the bar: "3 volumes share this space".
- **Locked state** (the designed one in `ui-design.md`): a password field in the content area, the hint under it when there is one, "Unlock" (Enter). `unlock` runs on the background executor; the field shows "Checking…" and is disabled meanwhile. `WrongPassword`: "That password did not unlock this volume. If this is the internal disk of a Mac with a T2 chip or Apple silicon, it can only be unlocked on that Mac." Success: the source becomes `Browsable` with the returned filesystem and the listing loads. The password string is dropped at once and never logged; the field's content is cleared on any outcome. This is the second text input in the app: reuse `components/search_field.rs`'s input core with a masked mode (dots), do not write another.
- **Snapshots:** when `fs.snapshots()` (asked on the background executor after mount) is non-empty, the sidebar shows a collapsed row "12 snapshots" under the volume; expanded, one row per snapshot by name and date, newest first. Selecting one calls `open_snapshot` in the background and makes a `Volume` source of it (same partition, `SourceKind::Volume`), cached for the session. Snapshots are never indexed automatically; search there builds on first use like host sources.

## 9. Encryption (`crypto.rs`) — the brief is §9.5; O builds

Scope: volumes whose `apfs_fs_flags` lack `0x1` (so: encrypted) and have `0x8` (one key), on a container with a key locker. Anything else encrypted is "can only be unlocked on the Mac it came from" or unsupported per-file encryption.

### 9.1 Key hierarchy

```
password ──PBKDF2-HMAC-SHA256(salt, iterations)──▶ 32-byte key
        ──RFC 3394 unwrap──▶ KEK   (from the volume keybag's unlock record)
KEK     ──RFC 3394 unwrap──▶ VEK   (from the container keybag's volume key record)
VEK = 32 bytes: AES-128-XTS, key1 = first 16 bytes, key2 = last 16
```

### 9.2 Keybags

- **Container keybag:** `nx_keylocker` gives a paddr and block count (cap 16 blocks). The blocks are encrypted with AES-XTS where both keys are the container UUID; sector size 512; the tweak of the first sector is `paddr × (block_size / 512)`, incrementing per sector. After decryption it is an object (type `'keys'`) that verifies like any other.
- Keybag body after the object header: version u16 (2), key count u16, byte count u32, 8 bytes padding, then entries: UUID 16 bytes, tag u16, key length u16, 4 bytes padding, key data; each entry padded to 16 bytes. Bounds: count ≤ 1024; each entry inside the byte count.
- Tags: `2` wrapped volume key (data: a key blob), `3` unlock records (in the container keybag the data is the location of the *volume* keybag: paddr u64, block count u64), `4` passphrase hint (UTF-8).
- In the container keybag, find the entries whose UUID is the volume's UUID: tag 2 → the VEK blob; tag 3 → the volume keybag's location.
- **Volume keybag:** same format, object type `'recs'`, encrypted with the *volume* UUID as both XTS keys. Its tag 3 entries are KEK blobs, one per user who can unlock (the entry UUID names the user; the personal recovery key is `EBC6C064-0000-11AA-AA11-00306543ECAC`); tag 4 entries are hints.

### 9.3 Key blobs

Blobs are DER-style tag-length-value. Apple's reference does not describe them; what follows is how `apfs-fuse` reads them (its behaviour, written from memory, **(added)** on 2026-11-12). The Julian-made encrypted fixture of §9.4 is the proof: if the fixture does not unlock, this section is what is wrong, and that is a stop-and-ask.

```
30 len                        the blob
  80 len  ..                  unknown (a version)
  81 20   32 bytes            HMAC-SHA256 over the A3 element (not checked: see below)
  82 08   8 bytes             salt of that HMAC
  A3 len                      the key record
    80 len  ..                unknown
    81 10   16 bytes          UUID (the volume's for a VEK blob, the user's for a KEK blob)
    82 08   8 bytes           flags: read the first 4 as a little-endian u32, `kind`
    83 len  24 or 40 bytes    the wrapped key (RFC 3394: 16 or 32 bytes of key plus 8)
    84 len  1–8 bytes         iterations, big-endian unsigned          (KEK blob only)
    85 10   16 bytes          PBKDF2 salt                              (KEK blob only)
```

Tags `80`–`85` are context-specific primitives and `A3` a context-specific constructed element; the reader does not need to know that, only the bytes. A minimal TLV reader: one-byte tags, definite lengths in short form (below `0x80`) or long form (`0x81`–`0x84` followed by that many length bytes), every length checked against the remaining input, depth ≤ 4, at most 16 elements per level, unknown tags skipped. Nothing else of ASN.1 is needed.

`kind` (believed):

| `kind` | Meaning | Unwrap |
|---|---|---|
| `0x00`, `0x10` | 256-bit key | wrapped key is 40 bytes; KEK = AES-256 unwrap with the 32-byte derived key; VEK = AES-256 unwrap with the KEK |
| `0x02` | 128-bit, converted from Core Storage | wrapped key is 24 bytes; AES-128 unwrap with the first 16 bytes of the derived key (KEK) or of the KEK (VEK). The 32-byte XTS key is then the 16 unwrapped bytes followed by the first 16 bytes of SHA-256(unwrapped ‖ the blob's UUID) |
| anything else | not password-derivable (a hardware-bound key) or unknown | skip the record |

When every unlock record is skipped for its `kind`, `unlock` fails with `Unsupported("this volume can only be unlocked on the Mac it came from")` and the app shows the designed sentence. When at least one record was tried and none opened, `WrongPassword`.

The HMAC is an integrity check of the blob, keyed from a fixed string and the 8-byte salt. It is **not verified**: RFC 3394's own check value already tells a right key from a wrong one, and a tampered blob can at worst make unlock fail. Fewer moving parts.

### 9.4 Unlock and decryption

`unlock(password)`: for each tag-3 record of the volume keybag (at most 64): derive (iterations capped at `MAX_ITERATIONS = 10_000_000`; zero is invalid), unwrap the KEK; RFC 3394's built-in check value (`A6A6A6A6A6A6A6A6`) tells a wrong password from a right one; on success unwrap the VEK with the KEK. All records fail → `WrongPassword`. Try the password both as typed and, when it looks like a recovery key (six groups of four separated by dashes), as is: the recovery key is just a password for its own record.

Decrypting, with `cs = block_size / 512`:

- **Metadata** (file-system tree nodes of this volume; not the object maps, not the superblocks): the block at `paddr` is decrypted in 512-byte sectors, sector `i` using tweak `paddr × cs + i`, then verified as usual. `Container::node(paddr, Some(key))` does this before the checksum.
- **File data:** a run with crypto id `c` covering blocks `p, p+1, …`: block `p + k` uses tweak base `(c + k) × cs`. (On a volume that was never re-encrypted, `c` equals the block number.) `ExtentSource` decrypts whole sectors: a read is widened to 512-byte boundaries into a scratch buffer, decrypted, and the wanted part copied out.
- XTS (IEEE 1619): the tweak is the sector number as a 128-bit little-endian value encrypted with key2; per 16-byte block, XOR-decrypt-XOR with key1, then multiply the tweak by α in GF(2^128) (shift left by one, XOR `0x87` into the low byte on carry). Sectors are whole multiples of 16 bytes, so ciphertext stealing is never needed. Test with the IEEE 1619 vectors.
- RFC 3394 unwrap over AES-256 (or AES-128 for converted keys): the six-round algorithm with the default IV. Test with the RFC's vectors.

Key material lives in a `VolumeKey` whose `Drop` overwrites it with zeros (a plain loop plus `std::hint::black_box`; no `zeroize` crate). Keys are never formatted with `Debug` (implement `Debug` by hand to print `VolumeKey(..)`), never logged, never saved.

### 9.5 The brief for O (Fable's half of task 13, written 2026-11-12)

Everything O needs beyond §9.1–§9.4. Paste this section and §9.1–§9.4 into the brief, with the current line numbers of `container.rs`, `volume.rs` and `mod.rs`.

**Files owned:** `apfs/crypto.rs` (new), and the marked call sites in `apfs/{container,volume,mod}.rs`. **Dependencies** (declare in the workspace table only after Julian's yes to README decision 9): `aes` 0.8, `sha2` 0.10, `hmac` 0.12, `pbkdf2` 0.12, the versions already in `Cargo.lock`.

```rust
// apfs/crypto.rs — pub(crate) unless marked
pub struct VolumeKey { key1: [u8; 16], key2: [u8; 16] }       // Drop zeroes; Debug prints "VolumeKey(..)"; not Clone
impl VolumeKey {
    /// Decrypts whole 512-byte sectors in place. `first_sector` is the tweak of the first one.
    /// `data.len()` must be a multiple of 512 (else `Corrupt`, never a panic).
    pub fn decrypt_sectors(&self, first_sector: u64, data: &mut [u8]) -> Result<()>;
}
/// AES-XTS with the same 16 bytes as both keys: how keybags are stored.
pub fn uuid_key(uuid: &[u8; 16]) -> VolumeKey;

pub struct Keybag { entries: Vec<KeybagEntry> }               // ≤ 1024 entries
pub struct KeybagEntry { pub uuid: [u8; 16], pub tag: u16, pub data: Vec<u8> }   // data ≤ 512 bytes
impl Keybag {
    /// `block` is one decrypted, verified keybag object (type 'keys' or 'recs').
    pub fn parse(block: &[u8]) -> Result<Self>;
}
pub struct KeyBlob { pub uuid: [u8; 16], pub kind: u32, pub wrapped: Vec<u8>, pub iterations: Option<u64>, pub salt: Option<[u8; 16]> }
impl KeyBlob { pub fn parse(der: &[u8]) -> Result<Self>; }

/// RFC 3394. `None` when the check value is wrong (a wrong key, not an error).
pub fn unwrap_key(kek: &[u8], wrapped: &[u8]) -> Option<Vec<u8>>;   // kek 16 or 32 bytes; wrapped 24 or 40

/// What mount keeps for a locked volume. Holds no secret.
pub struct Locked { container: Arc<Container>, superblock_paddr: u64, volume_uuid: [u8; 16], vek: KeyBlob, unlock: Vec<KeyBlob>, hint: Option<String> }
impl LockedVolume for Locked { /* hint(): from memory. unlock(): below. */ }
```

Decisions O should not have to make:

1. **At mount** (`mod.rs`), for a volume whose flags say encrypted with one key: read and decrypt the container keybag (§9.2), find this volume's tag-2 and tag-3 entries, read and decrypt the volume keybag, parse every blob, and return `VolumeState::Locked`. Any failure on that path is `VolumeState::Failed` with the error; the other volumes are unaffected. No key locker, or no tag-2 entry for the volume: `Failed(Unsupported("this volume can only be unlocked on the Mac it came from"))`.
2. **`unlock`** tries each unlock record with a usable `kind`, in keybag order, at most `MAX_UNLOCK_RECORDS = 64`. Per record: `iterations` must be 1 ..= `MAX_ITERATIONS`, and the sum over all records tried in one call must stay within `MAX_TOTAL_ITERATIONS = 50_000_000` (a hostile keybag must not buy minutes of CPU; past the budget the remaining records are skipped). The password is tried as its UTF-8 bytes as typed; if it has a non-ASCII character and fails on every record, once more in Apple's decomposed form (`hfsplus::unicode::decompose`, re-encoded as UTF-8), within the same budget. The derived key and the KEK live in arrays that are zeroed before the function returns on every path.
3. **Which blocks are encrypted** is read, not assumed: a virtual object is encrypted when its object map value has flag `0x4`. For a physical metadata block of an unlocked volume that has no map entry to ask (the snapshot metadata tree, a sealed file-extent tree), verify it as stored first; if the checksum fails, decrypt a copy with tweak `paddr × cs` and verify that. The checksum makes the guess safe. No-header nodes (§3.5) of an encrypted volume are a case no fixture covers yet: return `Unsupported` for a volume that is both sealed and encrypted.
4. **The node cache** is keyed by `(volume index, paddr)` for decrypted nodes, so a decrypted node of one volume is never served to another, and so a locked volume's nodes are never cached in clear under the shared key.
5. **`ExtentSource::read_at` with a key:** split the request by run; for each encrypted run widen to 512-byte sector boundaries inside the run, read into a scratch buffer of at most 1 MiB at a time, `decrypt_sectors(crypto_id × cs + sector_in_run, ..)`, copy the wanted part. A file's last block is stored whole, so widening never reads past the run. `read_bulk` takes the same path.
6. **Nothing secret in errors or logs.** `Error::WrongPassword` carries nothing. No `tracing` call in `crypto.rs` takes key material, the password, or a blob's bytes; log the record's UUID and `kind` at debug at most.
7. **`unsafe`: none.** The zeroing is a loop over the array followed by `std::hint::black_box(&array)`.

Bounds added to §11: `MAX_UNLOCK_RECORDS = 64`, `MAX_TOTAL_ITERATIONS = 50_000_000`, `MAX_KEYBAG_BLOCKS = 16`, `MAX_KEYBAG_ENTRIES = 1024`, `MAX_KEY_DATA = 512` bytes per entry, `MAX_TLV_DEPTH = 4`.

Done means: the vectors below pass; `apfs_keybag` fuzz target builds and its seeds run; the builder's encrypted container unlocks with its password and fails with another; clippy clean; then, with Julian's fixture, a known file reads back.

Tests: the standards' vectors (IEEE 1619 XTS-AES-128 vectors 1, 2 and 4; RFC 3394 §4.1 and §4.6; one PBKDF2-HMAC-SHA256 vector from RFC 7914 §11); a keybag and blob round trip built by the test builder with a known password; wrong password; a password with a non-ASCII character stored decomposed; a KEK blob with iterations 0, with 2^32−1, and a keybag whose records exceed the total budget; truncated TLVs at every byte; an unlock record of unknown `kind` alone (→ `Unsupported`) and beside a usable one. Fuzz target `apfs_keybag` over the keybag and blob parsers. A real encrypted fixture: Julian makes a 10 MB encrypted APFS image with a known password (`hdiutil create -fs APFS -encryption` is *image* encryption, which is not this; instead: create an APFS image, attach it, `diskutil apfs encryptVolume`, detach) and the test reads a known file from it.

## 10. Snapshots (`snapshot.rs`)

The snapshot metadata tree (`apfs_snap_meta_tree_oid`, physical) uses file-system-style keys: type 1 (snapshot metadata, the object id is the snapshot's xid) with value: extent-ref tree oid u64 @0, **superblock paddr u64 @8**, created u64 @16, changed u64 @24, inode number u64 @32, extent-ref tree type u32 @40, flags u32 @44, name length u16 @48, name @50. Type 11 records map names to xids and are not needed.

- `snapshots()`: walk type 1 records (cap `MAX_SNAPSHOTS = 65_536`), `id` = the xid.
- `open_snapshot(xid)`: read the volume superblock stored at the snapshot's superblock paddr (verify, type `0xD`); build a `Volume` from it that shares the *current* object map but resolves every virtual oid with `max_xid = xid`. That one parameter is the whole difference. A dataless snapshot (incompatible feature `0x2`) has no tree to read: `Unsupported`.
- The snapshot's `identity` uses the snapshot xid in `change`, and its `info.name` is "<volume> — <snapshot name>".

## 11. Bounds, in one place

| Constant | Value | Guards |
|---|---|---|
| `MAX_XP_DESC` | 65,536 blocks | checkpoint area scan |
| `MAX_EPHEMERAL` | 65,536 | checkpoint mappings kept |
| `MAX_LEVEL` | 16 | tree depth |
| `NODE_CACHE` | 4,096 nodes | memory (≤ 256 MiB at 64 KiB blocks; scale down to keep ≤ 64 MiB) |
| `OMAP_CACHE` | 65,536 per map | memory |
| `MAX_EXTENTS` | 4,000,000 per stream | memory at `open` |
| `MAX_XFIELDS` | 64 | extended-field parse |
| `MAX_SNAPSHOTS` | 65,536 | listing snapshots |
| `MAX_CIB` | 1,048,576 | space manager walk |
| `MAX_ITERATIONS` | 10,000,000 | key derivation time |
| `MAX_LINK_TARGET` | 4,096 bytes | symlink xattr |
| `MAX_UNLOCK_RECORDS` | 64 | unlock records tried per call |
| `MAX_TOTAL_ITERATIONS` | 50,000,000 | key derivation time across all records of one unlock |
| `MAX_KEYBAG_BLOCKS` / `MAX_KEYBAG_ENTRIES` / `MAX_KEY_DATA` | 16 / 1,024 / 512 bytes | keybag parse |
| `MAX_TLV_DEPTH` | 4 | key blob parse |

## 12. Testing

- `test_container.rs` (S): builds a container in memory with correct checksums: block 0 and one checkpoint (a map block and a superblock), a container object map with one leaf, one or more volumes each with an object map, a file-system tree of one root and several leaves (so index nodes and cursor climbing are exercised), files with extents, an embedded and a stream xattr, a compressed file, a symlink, a snapshot, a hashed and a non-hashed variant. It is also the source of fuzz seeds.
- Per-structure unit tests: one valid fixture, several malformed, as in HFS+.
- Stale-superblock test: block 0 at xid 5, the descriptor area holding xid 9 (valid) and xid 12 (bad checksum) → 9 is chosen.
- `scan` equals a recursive walk. Snapshot view differs from the live view in exactly the built difference.
- Damage sweeps in `end_to_end.rs` over the built container.
- Fuzz targets: `apfs_mount` (open, list volumes, walk, read, scan), `apfs_btree_node`, `apfs_keybag`.
- Real fixtures from Julian's Mac, stored as zlib `.dmg` files so they stay small and go through our own UDIF reader: a 2-volume container with a snapshot; a case-sensitive one; one written to and unplugged without ejecting (stale block 0). `tools/conformance/README.md` gains the `hdiutil`/`diskutil` commands. The conformance manifest script already works on any mounted volume.

## 13. Tasks, in order

| # | Owner | Task | Files owned |
|---|---|---|---|
| 1 | Lead | **Done** (see "Verification" at the top). What is left to confirm is listed there and belongs to the task that writes each parser | this file |
| 2 | Lead | **Done.** §7 API (`Mount`, `ContainerVolume`, `VolumeRole`, `LockedVolume`, snapshots on the trait, `WrongPassword`) and the app's one-line adaptation | `fs/probe.rs`, `fs/mod.rs`, `error.rs`, `state/sources.rs` |
| 3 | H | Stubs for §2; move `bytes.rs` and `compression.rs` to their shared homes with re-exports | listed |
| 4a | S | `object.rs` with Fletcher-64 | `apfs/object.rs` |
| 4b | S, parallel | `test_container.rs`, first version: container, one volume, one-leaf trees | `apfs/test_container.rs` |
| 5 | O | `btree.rs` | `apfs/btree.rs` |
| 6 | O | `container.rs`, `omap.rs` | those |
| 7 | O | `records.rs`, `volume.rs` without snapshots, sealed trees or keys; `mod.rs`; probe registration | those |
| 8a | S | `spaceman.rs`; builder grows to multi-leaf trees, xattrs, compression, symlinks | `apfs/spaceman.rs`, builder |
| 8b | S, parallel | §8 app: container volumes, strip grouping, sidebar order | `state/sources.rs`, `components/{sidebar,disk_strip}.rs`, `state/strip.rs` |
| 9 | O | Sealed volumes (fext tree); `snapshot.rs`; snapshots in the builder | `apfs/volume.rs`, `apfs/snapshot.rs` |
| 10 | S | Snapshot rows in the sidebar; fuzz targets and seeds; end-to-end and sweeps | app files, `fuzz/`, `end_to_end.rs` |
| 11 | Lead | Conformance against Julian's real containers; listing and scan timings | roadmap |
| 12 | Julian | Approve the four crypto crates | — |
| 13 | O | §9 `crypto.rs`, `DecryptingSource`, wiring `Locked`, from the brief in §9.5 (Fable's half is done) | `apfs/crypto.rs`, `apfs/{container,volume,mod}.rs` |
| 14 | S | Locked state UI with the masked field | `views/explorer.rs`, `components/search_field.rs`, `state/sources.rs` |

Tasks 1–11 are the roadmap's first four APFS lines; 12–14 are "password unlock" and may ship after M8 starts.

What Julian tries: a USB stick formatted APFS on a recent Mac; a full macOS system disk image (System and Data volumes, sealed system); a Time Machine APFS disk (many snapshots); a stick pulled without ejecting; an encrypted external volume with its password and with a wrong one.

## 14. Out of scope

Fusion, hardware-bound keys, per-file keys, encryption rolling, firmlinks between System and Data (each volume is browsed on its own), hash-keyed lookups, clone and space-sharing accounting, the reaper, writing anything.

## 15. Changes during build

- **Verified while writing the parsers (against `apfsprogs` headers and `apfsck` behaviour, read not copied, 2026-11-15).** Node flags `0x8` hashed and `0x10` no header, tree info flags `0x80` hashed and `0x100` no header: as §3.5 says. Key order (§3.7): names compare as plain bytes (C `strcmp`), length not first; a hashed directory key compares its hash bits (`name_len_and_hash & ~0x3FF`, length masked off) and then the name. Space manager offsets (§6): as written. Checkpoint map blocks carry `oid == paddr` and the checkpoint's xid, and node subtypes match their tree (`0xB`, `0xE`, `0x10`, `0x1F`); the reader checks all three, as `apfsck` does.
- **Tasks 3–6.** `apfs/bytes.rs` is APFS's own (little-endian); `hfsplus/bytes.rs` and `hfsplus/compression.rs` were not moved, because lanes only add lines to shared files: `volume.rs` calls `hfsplus::compression::open` directly. `Container` is split: `Blocks` (source, geometry clamped to the source, node cache) is what trees, cursors and `Omap::lookup` take, so the container object map opens before the `Container` exists and a cursor holds `Arc<Blocks>`. `Tree::seek` descends into the last child whose key is *less than* the target (not `<=`), so records equal to a partial target that begin in the previous child are found; `seek_le` uses `<=`. A cached node is checked against the asking tree's layout, header mode and oid, so a block shared by two trees on a damaged disk fails instead of crossing between them. Fuzz targets `apfs_mount` (open and resolve, grows into the full walk at task 7) and `apfs_btree_node`; entry points in `apfs::fuzzing` (`#[doc(hidden)]`).
