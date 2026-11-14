# M4. Search

Goal: type in the search field and the file list becomes results on every keystroke. Budgets (`performance.md`): first results in 16 ms over 1 million names and 50 ms over 5 million; index built in 5 s per million entries on HFS+; a saved index usable in 200 ms; about 80 bytes of memory per entry.

The model is unchanged from `performance.md`: all names in memory, a full parallel scan per keystroke. This file fixes the details that decide whether it meets the budget and stays correct on hostile volumes.

## 1. Decisions

1. **Entries are stored in depth-first pre-order.** After the scan, entries are permuted so every folder is followed directly by its whole subtree. Consequences, all wanted:
   - `parent[i] < i` for every entry but the root. A parent chain cannot loop, and a loaded index is proven loop-free by one linear check.
   - "Search in this folder" is a contiguous range of entries, so a scoped query scans less instead of filtering more.
   - Entries that are not reachable from the root (orphans, cycles made of directory hard links, children of hidden private folders) fall out for free: the walk never reaches them.
2. **`folded` has the same byte layout as `names`.** The fold lowercases a character only when its lowercase form is one character with the same UTF-8 length; otherwise the character is kept. So one offset array serves both buffers and a match range in `folded` is the highlight range in `names`. Cost: a handful of characters (`İ`, `K` KELVIN SIGN, `ẞ`) match only themselves. Not worth a second offset array.
3. **No memory map.** The saved index is read into memory and checked. 80 MB for a million entries reads in well under 200 ms from any SSD, and the engine keeps zero `unsafe`. `folded` is not saved; it is recomputed in parallel on load.
4. **Ranking is a 64-bit key per hit**, so ordering is an integer sort and the "top few thousand first" rule is `select_nth_unstable`.
5. **HFS+ `scan` reads leaf nodes in physical order but only nodes the B-tree's own allocation bitmap marks as used.** A freed node still holds its old records; reading it would index deleted files.
6. **Hard links are resolved inside the scan from the pass itself**, not by one catalog descent per link (a Time Machine volume has millions). Compressed sizes come from one sequential pass over the attributes tree for the same reason.
7. **Sorting results by a column happens in the engine** (`search/order.rs`) with Rayon, because a name sort of a million hits on one thread takes seconds.
8. Dependencies: `rayon` 1 and `memchr` 2 in `crates/lib`. Both are in the lock file through GPUI. Julian approves (README decision 1).

## 2. Trait additions (`fs/mod.rs`) — lead applies, then S implements `native`

```rust
/// One entry reported by [`FileSystem::scan`].
#[derive(Debug, Clone, Copy)]
pub struct ScanEntry<'a> {
    /// The node to open or list. For a hard link, the target.
    pub node: NodeId,
    /// The folder this entry is listed in.
    pub parent: NodeId,
    /// Display name, as a listing reports it.
    pub name: &'a str,
    pub kind: NodeKind,
    pub size: Option<u64>,
    pub resource_size: Option<u64>,
    pub modified: Option<Timestamp>,
    pub flags: EntryFlags,
}

/// What a scan covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// Entries passed to the visitor.
    pub entries: u64,
    /// Structures (B-tree nodes, folders) that could not be read and were skipped.
    pub skipped: u64,
}

/// Names a volume and the state it is in, for the saved search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VolumeIdentity {
    /// Stays the same for the life of the volume.
    pub id: [u8; 16],
    /// Changes whenever the volume's content may have changed.
    pub change: [u8; 16],
}

pub trait FileSystem {
    // ... existing methods ...

    /// Every entry on the volume except the root, in whatever order is fastest.
    ///
    /// Contract: children may arrive before their parents. A node may be
    /// reported more than once under different parents (hard links); for a
    /// folder, its children hang from its first report. The root is never
    /// reported; its children have `parent == self.root()`. Entries a listing
    /// hides (format-private folders) are not reported. The visitor returns
    /// `Break` to stop; the scan then returns `Ok` with what it had counted.
    /// A damaged structure is skipped and counted, not an error; `Err` is for
    /// I/O failure and for a volume that cannot be scanned at all.
    ///
    /// The default walks `read_dir` breadth-first from the root.
    fn scan(
        &self,
        visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
    ) -> Result<ScanSummary> { default_scan(self, visit) }

    /// The volume's identity and change marker. Answers from memory.
    /// `None` when the volume has none or may change while open (the host).
    fn identity(&self) -> Option<VolumeIdentity> { None }
}
```

`default_scan` (a free function in `fs/mod.rs`, generic over `?Sized`): a queue of folder ids starting at the root; a `HashSet<NodeId>` of folders already queued (the cycle guard); for each folder `read_dir` and `next_batch(.., 4096)` until `Done`; every entry is visited with `parent` = the folder; browsable `Directory` entries not yet in the set are queued. Symbolic links are reported and never followed. A `read_dir` or `next_batch` error adds 1 to `skipped` and moves on (entries already appended by the failing call are still visited). Bound: `MAX_SCAN_FOLDERS = 50_000_000` queued folders, then `Err(Corrupt(LimitExceeded))`.

`NativeFs` keeps the defaults (no identity; default scan).

## 3. HFS+ scan (O)

### 3.1 `BTree::scan_leaves` (`hfsplus/btree.rs`)

```rust
pub struct LeafScan { pub nodes: u32, pub skipped: u32 }

impl BTree {
    /// Calls `visit` with every allocated leaf node, in node-number order.
    pub fn scan_leaves(
        &self,
        visit: &mut dyn FnMut(&Node) -> Result<ControlFlow<()>>,
    ) -> Result<LeafScan>;
}
```

1. **Bitmap.** Read node 0. Its third record (index 2) is the map record: one bit per node, most significant bit first, bit set = node in use. If node 0's forward link is non-zero, follow the chain of map nodes (`kind == 2`); each has one record of more bitmap. Collect into a `Vec<u8>`. Bounds: at most `total_nodes / 8 + 1` bytes are kept; at most `total_nodes` map nodes are followed; a map node whose kind is not 2 or whose link repeats ends the chain. If the bitmap covers fewer than `total_nodes` nodes, nodes past it are treated as *used* (reading a stale node is better than dropping live files). If node 0 itself cannot be read, fail.
2. **Pass.** Read the tree's fork with `read_bulk` in pieces of `SCAN_PIECE = 1 MiB` rounded down to a whole number of nodes (at least one node). For each node-sized slice whose bit is set: build the validated `Node` view from the bytes (expose the existing private `Node::parse` to the module as needed; do not read the node again through `read_node`). `kind != Leaf` is skipped silently. A node that fails validation adds 1 to `skipped`. A leaf is passed to `visit`; `Break` ends the pass.
3. An I/O error on a piece is retried node by node for that piece so one bad sector costs one node; nodes that still fail are skipped and counted. If more than `MAX_SKIPPED = 4096` nodes are skipped, stop with the I/O error.

Tests: a tree built with the existing `btree::fixtures` where a freed leaf (bit clear) still holds records and must not be visited; a map-node chain; a bitmap shorter than the tree; a looping map chain; a corrupt leaf in the middle.

### 3.2 `HfsPlus::scan` (`hfsplus/mod.rs` plus a new private `hfsplus/scan.rs`)

Three steps, all sequential I/O.

**Step A, compressed sizes.** Only when the volume has an attributes tree. `scan_leaves` over it; for each record whose key name is `com.apple.decmpfs` and whose record type is inline data (`0x10`), parse the first 16 bytes of the value with `DecmpfsHeader::parse` and insert `file_id → uncompressed_size` into a `HashMap<u32, u64>`. Records that fail to parse are ignored. Bound: stop inserting at `MAX_COMPRESSED = 50_000_000` entries.

**Step B, catalog pass.** `scan_leaves` over the catalog. For each record of each leaf (a record that fails to parse is skipped and counted, the rest of the node still read):

- Thread records: ignored.
- Parent is the file-link private folder (`links.file_folder()`): the record is a link target. Parse the number out of the name `iNode<decimal>`; insert `number → Target { node, kind, size, resource_size, modified, flags }` into `file_targets`. Do not visit.
- Parent is the directory-link private folder: same with `dir_<decimal>` into `dir_targets`. Do not visit. (Its children have its CNID as parent and are visited normally.)
- The record *is* one of the two private folders (`links.is_private_folder(id)`): skip.
- A file record that `link_of` recognises as a link: push `PendingLink { parent, name (into one shared String arena, by range), link_ref, kind, hidden, own_id }`. Do not visit yet.
- Anything else: visit with the same fields a listing gives: `kind_of_mode`, `entry_flags`, size from the compressed map when `is_compressed(file)` (missing from the map → `None`), `COMPRESSED` flag, `resource_size_of`, `hfs_time(content_mod_date)`. Name through the same compose path as `CatalogDir`, into a reused buffer.

Root-level names hidden by §1.4 of `m3-leftovers.md` follow whatever the listing does; use one shared predicate.

**Step C, links.** For each pending link: look up its reference in `file_targets` or `dir_targets`. Found: visit with the link's `parent`, name and hidden flag and the target's node, kind, size, dates and other flags (what `through_links` does). Not found: visit as a file with `node = own_id`, no size, as the listing shows an unresolvable link.

Bounds: `MAX_PENDING_LINKS = 50_000_000`; past it, further links are visited unresolved. The maps are dropped before returning.

`ScanSummary.skipped` is the sum of skipped nodes and skipped records.

**`HfsPlus::identity`.** Computed at mount from the replayed header and stored in the struct:

- `id`: `finder_info[6]` and `finder_info[7]` (the 64-bit volume identifier) as 8 big-endian bytes, then `create_date`, then `block_size`, each 4 bytes big-endian. A volume whose identifier is zero still gets an identity from the last 8 bytes.
- `change`: `write_count`, `modify_date`, `file_count`, `folder_count`, 4 bytes each.

Tests: build with `test_volume` a volume holding plain files, a compressed file, a file hard link with two names, a directory hard link, a deleted file in a freed node. Assert the visited set equals a recursive `read_dir` walk of the same volume (same names, parents, sizes, flags), which is the real specification of `scan`. Add `scan` to the `hfsplus_mount` fuzz target after the existing walk.

Measurement: `hfsplus_scan_throughput` (ignored test): a `test_volume` with 200,000 files in memory, print entries per second. Target: over 400,000 entries/s in release, which leaves margin under the 5 s budget.

## 4. The index (`search/index.rs`) — O

```rust
pub struct Index {
    names:    Box<[u8]>,   // every name, each followed by one 0 byte
    folded:   Box<[u8]>,   // same length and offsets as `names`
    offsets:  Box<[u32]>,  // len = entries + 1; name i is names[offsets[i] .. offsets[i+1] - 1]
    parent:   Box<[u32]>,  // entry index of the parent; entry 0 is the root and its own parent
    node:     Box<[u64]>,
    size:     Box<[u64]>,  // NO_SIZE (u64::MAX) = unknown or a folder
    modified: Box<[i64]>,  // NO_TIME (i64::MIN) = unknown
    rsize:    Box<[u32]>,  // resource fork bytes, saturated at u32::MAX - 1; NO_RSIZE (u32::MAX) = none
    flags:    Box<[u16]>,  // IndexFlags
    identity: Option<VolumeIdentity>,
    summary:  ScanSummary,
}
```

- 38 bytes of arrays per entry plus twice the name. With 20-byte names that is 80 bytes: the budget. Do not add a field without removing one.
- **Entry 0 is the root**, with the volume's name, `parent[0] == 0`.
- `IndexFlags` (u16): bits 0–1 kind (0 file, 1 folder, 2 symlink, 3 other); bit 2 hidden; bit 3 compressed; bit 4 links to a folder; bit 5 has a non-empty resource fork; bits 8–11 a `KindClass` (below), filled at build from the extension.
- Limits: `MAX_ENTRIES = u32::MAX - 1`, `MAX_NAME_BYTES = u32::MAX` total. The builder stops accepting entries at either and marks the index incomplete (`summary.skipped += 1` per dropped entry).
- A NUL byte inside a name is stored as `0x1A`. Same length, and the separator stays unique.
- Accessors: `len()`, `name(i) -> &str`, `entry(i) -> Option<DirEntry<'_>>` (the same borrowed view listings use, so the app's rows need no second code path), `parent(i)`, `is_folder(i)`, `subtree(i) -> Range<u32>`, `crumbs(i) -> Vec<(NodeId, String)>` (root first, bounded by `MAX_DEPTH = 4096`), `folder_path(i, out: &mut String)` (the parent's path with ` › ` or `/` chosen by the caller: take a separator argument), `find_node(NodeId) -> Option<u32>` (linear over `node`, used once per scope change, not per keystroke).
- `subtree(i)`: for a non-folder, `i..i+1`. For a folder: `j = i + 1; while j < len && parent[j] >= i { j += 1 }`; the range is `i..j`. Correct because of pre-order (the first entry after a subtree is a child of an earlier ancestor). Linear in the subtree, about 1 ms per million entries; call it off the UI thread.

### 4.1 `fold` (`search/fold.rs`) — S

```rust
/// Appends the folded form of `name` to `out`. Always appends exactly `name.len()` bytes.
pub fn fold_into(name: &str, out: &mut Vec<u8>);
pub fn fold(text: &str) -> String;   // for queries
```

ASCII fast path: bytes below 0x80 go through `to_ascii_lowercase`. Otherwise per `char`: take `c.to_lowercase()`; if it yields exactly one char with the same `len_utf8()`, append it; else append `c` unchanged. Tests: length equality over every scalar value (a loop over `0..=0x10FFFF`), ASCII, Latin-1, Greek, Cyrillic, the three exceptions named in §1.

### 4.2 `KindClass` (`search/kind.rs`) — S

`enum KindClass { Other = 0, Folder, Document, Image, Audio, Video, Archive, Application, Code, DiskImage, Font }` from the extension, case-insensitive, by a sorted static table and binary search. Lists (extend freely, keep sorted): Document `doc docx odt pages pdf rtf txt md xls xlsx numbers csv ppt pptx key epub`; Image `png jpg jpeg gif bmp tif tiff webp heic psd pict pct icns svg raw cr2 nef`; Audio `mp3 m4a aac wav aif aiff flac ogg mid`; Video `mov mp4 m4v avi mkv mpg mpeg wmv webm`; Archive `zip sit sitx hqx bin gz tgz bz2 xz 7z rar tar cpgz`; Application `app exe` and any folder named `*.app`; Code `c h cpp rs py js ts html css json xml plist sh swift m java`; DiskImage `dmg iso img cdr sparseimage sparsebundle toast dsk`; Font `ttf otf ttc dfont suit`. Folders are `Folder` unless `*.app`.

### 4.3 Building (`search/build.rs`) — O

```rust
pub struct BuildProgress { pub entries: u64 }

pub fn build(
    fs: &dyn FileSystem,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(BuildProgress),   // called about every 16,384 entries
) -> Result<Option<Index>>;                     // Ok(None) when cancelled
```

1. **Collect** through `fs.scan` into temporary arrays in arrival order: names buffer, name ends, `node`, `parent_node` (the `NodeId`), and the value arrays. The visitor checks `cancel` every 4096 entries and returns `Break`.
2. **Resolve parents.** Build `HashMap<u64, u32>` from `node` to arrival index for *folders only*, keeping the first arrival of each node (decision 1 and the scan contract). For each entry, `parent_arrival = map[parent_node]`, or `ROOT` when `parent_node == fs.root()`, or `ORPHAN` when absent.
3. **Children lists** by counting sort: count children per parent, prefix-sum into `child_start`, fill `children`. O(n), two `u32` arrays.
4. **Pre-order walk** from the root with an explicit stack (no recursion; depth can be hostile), assigning new indices in visit order. A `visited` bit per arrival guards against a folder reached twice (cannot happen in a tree, but the input is hostile: a folder that is its own ancestor through the first-arrival map). Entries never visited are dropped and counted in `skipped`.
5. **Permute** every array into the new order, writing `names` and `offsets` fresh; then `folded` in parallel (`par_chunks` over entry ranges of 65,536, each writing its own slice of a pre-sized buffer through `split_at_mut`-style partitioning; no `unsafe`).
6. Peak memory is about twice the final index. Acceptable; say so in the module docs.

Tests: a hand-built `FileSystem` double that reports children before parents, a duplicated folder node, an orphan, a two-folder cycle not attached to the root, a NUL in a name, an empty volume (index of one entry, the root). Property: for every entry `parent[i] < i` or `i == 0`; `subtree` of the root is everything; `crumbs` of each entry round-trips through `lookup` on the double.

## 5. Queries (`search/query.rs`) — O

```rust
pub struct Query {
    pub text: String,                 // as typed
    pub scope: Option<u32>,           // a folder entry: search inside it only
    pub kinds: KindSet,               // bit set over KindClass; empty = any
    pub size: Option<RangeInclusive<u64>>,
    pub modified: Option<RangeInclusive<i64>>,   // unix nanoseconds
    pub include_hidden: bool,         // default false
}

pub struct Hits {
    keys: Vec<u64>,        // rank key, see below; the low 32 bits are the entry
    sorted: usize,         // keys[..sorted] are in final order
    by_entry: Vec<u32>,    // the same hits ascending by entry, for narrowing
    pub elapsed: Duration,
}

impl Index {
    /// `None` when `cancel` was set before the scan finished.
    pub fn search(&self, query: &Query, previous: Option<(&Query, &Hits)>, cancel: &AtomicBool)
        -> Option<Hits>;
}
impl Hits {
    pub fn len(&self) -> usize;
    pub fn entry(&self, position: usize) -> Option<u32>;
    pub fn sorted_len(&self) -> usize;
    /// Sorts everything. Call on a background thread when the view nears `sorted_len()`.
    pub fn sort_all(&mut self);
    pub fn entries_in_order(&self) -> Arc<[u32]>;   // the listing's `order`
}
pub fn match_ranges(name: &str, terms: &[String], out: &mut Vec<Range<usize>>);  // for bold
pub fn terms(text: &str) -> Vec<String>;   // folded, split on whitespace, deduplicated, empty removed
```

**Plan.**

1. `terms(text)`. No terms and no filter set: return an empty `Hits` (the app shows the folder, not the whole volume).
2. Range: `scope.map(subtree)` or `1..len` (the root is never a hit).
3. **Narrowing.** Use `previous` when the scope, filters and `include_hidden` are equal, every previous term is a substring of some new term, and `previous.len() * 4 < range.len()`. Then the candidates are `previous.by_entry` instead of the range; each is tested with step 5 directly.
4. **Scan.** Primary term = the longest (ties: the first). Split the range into chunks of about 1 MiB of `folded` at entry boundaries (walk `offsets`). `par_iter` over chunks; each worker checks `cancel` first, then runs `memchr::memmem::Finder` (built once, shared) over its slice of `folded`. For a hit at byte `p`: advance a local entry cursor forward to the entry containing `p` (gallop: double the step while `offsets[e + step] <= p`, then binary search inside the last step; never search from the chunk start). A needle has no 0 byte, so a match cannot straddle two names. Test the entry (step 5); then continue the search from the start of the next entry, so a name is examined once however often the term occurs.
   - No terms but filters set: the workers iterate entries and test filters only.
5. **Test an entry.** Hidden unless `include_hidden`; kind set; size and date ranges (unknown values fail a set range); every other term found in the entry's `folded` slice with `memmem::find`.
6. **Rank.** For the *first term as typed* (not the longest), over all its occurrences in the name, the best class: 0 the whole name equals the term; 1 the name starts with it; 2 an occurrence starts at a word boundary (the preceding byte is not ASCII alphanumeric and is below 0x80, or the occurrence is preceded by a lowercase-to-uppercase step in `names`); 3 anything else. Folders rank ahead of files inside a class. Key: `class << 60 | (is_file as u64) << 59 | min(name_len, 0x7FF) << 48 | 0 << 32 | entry`. Bits 32–47 are zero and reserved. Ascending key order is the result order: best class, folders first, shorter names first, then volume order (which is path order, thanks to pre-order).
7. **Collect.** Each chunk returns its `Vec<u64>` (already ascending by entry). Concatenate in chunk order to get `by_entry` for free. Then `keys`: if `len <= HEAD = 4096`, sort all; else `select_nth_unstable(HEAD)` and sort the head, `sorted = HEAD`.
8. Check `cancel` once more before returning.

**Cost check, to confirm with the benchmark.** One million names are about 21 MB of `folded`; `memmem` runs at several GB/s per core, so a rare term costs 1–3 ms on 8 cores. The worst case is a one-letter query that hits most names: one million entry tests at roughly 50–100 ns is 50–100 ms of CPU, 8–15 ms across 8 cores. That is why the gallop and "skip to the next entry" matter, and why step 6 must not allocate.

**Ordering by a column (`search/order.rs`) — O.**

```rust
pub enum HitOrder { Name, Modified, Size, ResourceSize, Kind, Folder }
pub fn order_hits(index: &Index, hits: &Hits, by: HitOrder, descending: bool, cancel: &AtomicBool)
    -> Option<Arc<[u32]>>;
```

`par_sort_unstable_by` over a copy of `by_entry`. Folders first, as in listings. Name order: compare `folded` slices with digit runs as numbers (the rule of `state/order.rs::compare_tokens`, reimplemented over bytes: equal non-digit prefixes, then digit runs compared by stripped length then bytes). Ties broken by entry. `Folder` compares parents' entry numbers, which is path order. Check `cancel` before and after the sort (a sort cannot be interrupted; at 5 million hits it is under a second on 8 cores).

**Measurements (S, `search/bench.rs` as ignored tests).** `synthetic(n, seed)` builds an `Index` through the builder from a deterministic generator: a 2,000-word list, 1–4 words per name, an extension from §4.2, folders of 1–200 entries, depth to 12. Print for n = 1M and 5M: build time, bytes per entry, and query times (median of 20) for a rare 8-letter term, a common 3-letter term, one letter, two terms, a scoped query, and a narrowing step. These lines go in the roadmap entry. The 16 ms and 50 ms budgets apply to the rare and common cases; the one-letter case is reported, with a note if it exceeds them.

## 6. Saved index (`search/persist.rs`) — O

File: little-endian, one header then sections in a fixed order.

```
0   8   magic "DATLSIDX"
8   4   version = 1
12  4   entries (u32)
16  8   names_len (u64)
24  16  identity.id
40  16  identity.change
56  8   summary.entries      64  8  summary.skipped
72  8   file_len (the whole file, checked against the real length)
80      names[names_len] · offsets[(entries+1)×4] · parent[entries×4] · node[entries×8]
        · size[entries×8] · modified[entries×8] · rsize[entries×4] · flags[entries×2]
```

```rust
pub fn save(index: &Index, out: &mut dyn Write) -> Result<()>;          // only when identity is Some
pub fn load(input: &mut dyn Read, file_len: u64, expect: VolumeIdentity) -> Result<Option<Index>>;
```

`load` returns `Ok(None)` for "not ours or not current" (wrong magic, version, identity or change marker) and `Err(Corrupt)` for a file that claims to match and is malformed. It treats the file as hostile:

- `file_len` equals the header's and equals `80 + names_len + entries × 38 + 4` exactly, computed with checked arithmetic, before any allocation. So allocation is bounded by the real file size.
- Sections are read into byte vectors and converted with `chunks_exact(..).map(from_le_bytes)`.
- `offsets[0] == 0`, strictly ascending, `offsets[entries] == names_len`, and the byte before each offset (but the first) is 0.
- `names` is valid UTF-8 (one `from_utf8` over the buffer; the 0 separators are valid UTF-8).
- `parent[0] == 0` and `parent[i] < i` for all others; every parent is a folder by `flags`.
- Then `folded` is recomputed in parallel.

The app writes to `<name>.tmp` and renames, so a crash never leaves a half file under the real name.

Fuzz target `search_index_load` (S): bytes → `load` with a fixed identity taken from the first 32 identity bytes of the input, then run one query on a loaded index. Seed: a saved 50-entry index.

Measurement: `index_load_time` (ignored): save a 1M synthetic index to a temp file, print load time. Budget 200 ms. If it misses on Julian's machine by more than 2x, that is the trigger to revisit decision 3, not before.

## 7. App

### 7.1 State (`state/search.rs`, new) — lead writes the skeleton, S fills it

```rust
pub enum IndexState {
    Absent,
    Building { entries: u64 },
    Ready(Arc<Index>),
    Failed(SharedString),
}

pub struct Search {                       // GPUI entity, owned by Workspace
    indexes: HashMap<SourceId, IndexState>,
    build_tasks: HashMap<SourceId, (Task<()>, Arc<AtomicBool>)>,
    query: Query, text: SharedString,
    generation: u64,
    cancel: Arc<AtomicBool>,              // replaced on every keystroke; the old one is set
    last: Option<(Query, Arc<Hits>)>,     // for narrowing
    active: bool,
}
```

- **When an index is built.** For a browsable `Volume` source: as soon as it is mounted, on the background executor: try `persist::load` from the cache file; otherwise `build`, then `save`. For a `Host` source: only on the first search there (README decision 3), never saved. One build at a time across sources (a queue), so two disks do not fight for the same USB bus; the current source's build jumps the queue.
- **Cache location** (`state/search.rs`, a pure function tested per platform): Windows `%LOCALAPPDATA%\DiskAtlas\index`, macOS `~/Library/Caches/DiskAtlas/index`, Linux `$XDG_CACHE_HOME/disk-atlas/index` or `~/.cache/disk-atlas/index`. File name: the 16 `id` bytes in hex plus `.idx`. After a save, delete the oldest files beyond `MAX_SAVED = 32` or 2 GiB total. Environment variable missing: no saving, no error shown.
- **A keystroke** (`Search::set_text`): set the old `cancel`, make a new one, `generation += 1`, spawn on the background executor `index.search(&query, last, &cancel)`. On return: drop if the generation is stale or the result is `None`; else store `last`, and hand `Listing` the results. No debounce: the scan is cheaper than a timer.
- While the index is `Building`, the query is stored and run when the index arrives. The field shows "Indexing… 312,000 items".
- **Scope.** Default: the whole volume. The "where" menu offers "This volume" and "This folder" (the folder shown when search began); the latter sets `Query::scope` through `find_node`.

### 7.2 `Listing` shows results in place — O (hot path)

`Listing` today holds `rows: Rows` (pages) and `order: Arc<[u32]>` of arrival indices. Generalise the row source and keep everything else:

```rust
enum RowSource {
    Folder(Rows),
    Index(Arc<Index>),          // an entry number plays the part of the arrival index
}
impl RowSource { fn get(&self, id: u32) -> Option<DirEntry<'_>> }
```

- `Listing::show_results(index: Arc<Index>, hits: Arc<Hits>, terms: Arc<[String]>, cx)`: on the first call, move the folder's `rows`, `order`, `selection`, cursor and scroll anchor into `stashed: Option<FolderView>`; then `rows = RowSource::Index`, `order = hits.entries_in_order()`, selection reset, cursor none, `notify`. Later calls just replace `order` and reset the selection.
- `Listing::leave_results(cx)`: restore `stashed` exactly. Escape is therefore instant and the user's selection in the folder is as they left it. A navigation while in results (Enter on a folder result, a sidebar click) drops the stash and loads normally.
- `Selection` is a bit set indexed by row id and grows on demand, so entry numbers up to 5 million cost 600 KB at worst. No change needed there.
- `Listing::set_sort` in results mode: `Relevance` is a new `SortColumn` shown only in results and is the default; another column calls `order_hits` on the background executor under the listing's generation.
- When the view's last visible position passes `hits.sorted_len() - 512`, the explorer asks `Search` to `sort_all` on the background executor (on a clone of the keys) and swap the order in under the same generation. Selection survives because it is by entry, not position.
- Extra data for results rows: `Listing::folder_of(position) -> SharedString` builds the parent path through `Index::folder_path` and caches it in a `HashMap<u32, SharedString>` keyed by parent entry, cleared when the index changes, capped at 4096 entries.
- **Activation.** Enter on a folder result navigates to it: `Location` from `Index::crumbs`. Enter on a file result navigates to its parent with the file as the focus name (M5 turns Enter on a file into Quick Look everywhere; then "Show in enclosing folder" becomes Ctrl+Enter). The details pane, the disk strip marks and (in M5) copy and Quick Look work unchanged because `Listing::primary()` still returns a `DirEntry` with a real `NodeId`.

### 7.3 Views — S

Verify first in the GPUI 0.2.2 source: `StyledText::with_highlights` (`elements/text.rs:188`) and how an editable single-line text field is built in this version (the `input` example under the registry's `gpui-0.2.2/examples`). The search field is the app's first text input; build it as its own component, `components/search_field.rs`, from that example, and nothing else in this milestone may grow a second one.

- **Search field** in the top bar: placeholder "Search <volume name>"; Ctrl+F focuses it and selects its text; Escape clears and leaves search, or, when already empty, returns focus to the list. Down or Enter from the field moves focus to the list on the first row. Inside the field at the right, in `ink3`: "1,204 results · 3 ms", or the indexing text.
- **Filter row** (`components/toolbar.rs`), shown only while search is active, 26 px controls: Where (This volume, This folder), Kind (the `KindClass` names, multi-select), Size (Any, under 1 MB, 1–100 MB, 100 MB–1 GB, over 1 GB), Date modified (Any, today, last 7 days, last 30 days, this year, before this year), and a "Hidden items" toggle. An active filter is filled with `ink` and shows its value. Menus are simple popovers built with `anchored()`/`deferred()`; keyboard: arrows, Enter, Escape.
- **Rows.** Results add a Folder column after Name (flexible width, `ink2`, middle-truncated). The matched ranges of the name are weight 700: `match_ranges(name, terms)` per visible row, into a reused buffer.
- **Footer.** Index state: "index up to date", "indexing… N items", "search unavailable (reason)". With skipped structures: "index incomplete (N unreadable)".
- **Empty results:** "No names contain “query”." With filters active, a second line: "Filters are on." and a "Clear filters" button.
- Keymap: add `Search` (Ctrl+F) and route Escape; update the table in `ui-design.md`.

## 8. Tasks, in order

| # | Owner | Task | Files owned | Done means |
|---|---|---|---|---|
| 1 | Julian | Approve `rayon`, `memchr` | — | — |
| 2 | Lead | Add `ScanEntry`, `ScanSummary`, `VolumeIdentity`, `scan`, `identity`, `default_scan` to the trait; declare the two dependencies | `fs/mod.rs`, `Cargo.toml`s | workspace check |
| 3 | H | Stubs with GPL headers: `search/{index,fold,kind,build,query,order,persist,bench}.rs`, `hfsplus/scan.rs`, `state/search.rs`, `components/search_field.rs`, and their `mod` lines | those | check |
| 4a | O | §3 HFS+ scan | `hfsplus/btree.rs`, `hfsplus/scan.rs`, `hfsplus/mod.rs` | tests in §3, clippy |
| 4b | S, parallel with 4a | §4.1 fold, §4.2 kinds | `search/fold.rs`, `search/kind.rs` | tests |
| 5 | O | §4 index and build | `search/index.rs`, `search/build.rs`, `search/mod.rs` | tests in §4.3 |
| 6 | O | §5 query and order | `search/query.rs`, `search/order.rs` | unit tests: every rank class, multi-term, each filter, scope, narrowing equals a fresh search, cancellation returns `None` |
| 7a | O | §6 persist | `search/persist.rs` | round trip; each validation rule has a failing fixture |
| 7b | S, parallel with 7a | Benchmarks and synthetic generator; fuzz target and seed | `search/bench.rs`, `fuzz/` | prints the figures |
| 8 | Lead | Run the benchmarks; compare with budgets; record in roadmap | — | — |
| 9 | O | §7.2 `Listing` row source and results mode | `state/listing.rs`, `state/loader.rs` | existing listing tests green; new tests for stash and restore |
| 10 | S | §7.1 `Search` entity, cache paths, build queue | `state/search.rs`, `app.rs` | tests for cache path and pruning |
| 11 | S | §7.3 field, filter row, columns, highlights, footer, keymap | `components/{search_field,toolbar,titlebar,file_row,statusbar}.rs`, `views/explorer.rs`, `keymap.rs` | Julian looks |
| 12 | H | `atlas_check --index`: build, print entries, skipped, time, bytes per entry | `examples/atlas_check.rs` | runs on the test volume |

What Julian tries at the end: open an image, see "indexing…" then "index up to date"; Ctrl+F, type, results on every key with bold matches and a Folder column; filters; Escape returns to the folder with the selection intact; close and reopen the image and search is ready at once.

## 9. Out of scope

Fuzzy matching, content search, regular expressions, query syntax, updating an index in place (volumes are read-only images or disks; the change marker handles it), searching several volumes at once.

## 10. Changes during build

Lead decisions made while building M4 (Opus 5.5 as lead, 2026-11-11). None changes a public interface, data layout, threading rule or security bound beyond what is written here.

1. **`default_scan` bounds a stream that never ends.** A folder whose stream answers `More` without adding an entry `MAX_EMPTY_BATCHES = 64` times in a row (the app loader's bound) is abandoned and counted in `skipped`. Without it a hostile or buggy `DirStream` hangs the build. Directory-kind entries are queued; symbolic links never are, whatever their `LINKS_TO_DIRECTORY` flag.
2. **`KindSet` lives in `search/kind.rs`** next to `KindClass` (the design names it in `Query` without placing it), as a `u16` bit set with `admits(class)` (empty admits everything). `KindClass` gets `ALL`, `bits`/`from_bits` for the flag bits 8–11, and `label` for the filter menu.
3. **Verify first, GPUI 0.2.2 (§7.3).** Confirmed: `StyledText::with_highlights(impl IntoIterator<Item = (Range<usize>, HighlightStyle)>)` at `src/elements/text.rs:188`; ranges must be on char boundaries (debug assertion). The editable field pattern is `examples/input.rs`: an entity implementing `EntityInputHandler`, a custom `Element` that shapes one line and calls `window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx)` in `paint`. That example moves the cursor by grapheme clusters through `unicode-segmentation`, which is not an app dependency; the search field moves by `char` boundaries instead (no new dependency). Grapheme movement can come later if Julian approves the crate.
4. **HFS+ scan (§3), as built.** Verify-first items (a)–(d) agree with TN1150 and `filesystems.md`. Choices where §3 was open:
   - The root's own folder record (keyed by parent 1) is never reported.
   - `MAX_SKIPPED` counts nodes that failed to *read*; nodes that fail validation are counted in `skipped` but never end the pass. Non-leaf nodes are recognised by the kind byte before validation, so a damaged index node is not counted.
   - Used nodes past the end of a short fork (and a partial last node) count as skipped.
   - An attributes tree that cannot be scanned at all costs the compressed sizes not yet reached and one `skipped`; the catalog pass still runs (a listing would not fail either).
   - Link target maps are capped at `MAX_PENDING_LINKS` each; the first target in node order wins. A target name must equal `TargetName(prefix, n)` under the catalog's own compare (no leading zeros), as `find` would see it.
   - Known gap: a decmpfs attribute stored as fork data (`0x20`) gives size `None` in the scan while the listing reads its size. xnu writes decmpfs inline on HFS+; revisit if the conformance pass shows otherwise.
   - The root-hiding rule is one method, `LinkResolver::hidden_in_listing`, used by `CatalogDir`, `lookup` and `scan` (§1.4 of `m3-leftovers.md` adds names there).
   - Test fixture change: `btree::fixtures::header_node` now marks nodes `0..total_nodes` used in the map record (it was all zeros, "all free"), so built volumes and the fuzz seeds have realistic bitmaps.
   - `hfsplus_scan_throughput` (see 5 for the index): 2,343,004 entries/s in release (200,000 files, in-memory image, 85 ms best of 5). Real-disk throughput is bounded by sequential read speed; not yet measured.
5. **Index and build (§4, §4.3), as built.**
   - `names` is held as `Box<str>` (private), so `name(i)` slices without a UTF-8 check or `unsafe`; `names_bytes()` still gives the bytes. The sibling modules build an index through `Index::from_parts(IndexParts { .. })`, which computes `folded` itself and assumes (debug-asserts through `Index::verify`) the invariants persist checks. `verify` is callable in release.
   - Values that collide with a sentinel are nudged: a size of `u64::MAX` is stored as `u64::MAX - 1`, a time of `i64::MIN` as `i64::MIN + 1`; resource sizes saturate at `u32::MAX - 1`.
   - `MAX_ENTRIES` counts the root and `MAX_NAME_BYTES` counts separators and the root's name. Once either limit is hit every later entry is dropped, so the result does not depend on name lengths after the limit.
   - `crumbs` is empty for an entry deeper than `MAX_DEPTH` (it cannot be navigated to); `folder_path` starts with `…` and keeps the nearest `MAX_DEPTH` folders, and appends to `out` without clearing it. The root's kind class is `classify(volume_name, true)`; its size and date are unknown.
   - Progress is also reported once when the scan ends, with the final count. A folder entry whose node is the root becomes an empty leaf.
   - `index_build_throughput` (builder only, in-memory double, 1,000,001 entries, release): 151–167 ms, about 6 million entries/s; 91.2 bytes per entry at 25.6-byte average names (38 + 2 × (25.6 + 1)), which is 80 at the budget's 20-byte names.
6. **Query and order (§5), lead decisions in the brief.**
   - "A filter set" means scope, kinds, size or modified. `include_hidden` alone widens results and is not a filter, so it does not turn an empty query into a whole-volume listing.
   - A scope that is not a folder, or out of range, gives no hits. The scope folder itself is never a hit.
   - `Hits` carries a private flag saying it came from a real plan; the empty `Hits` of "no terms, no filters" is never a narrowing base (it would otherwise narrow every later query to nothing).
   - In narrowing, every term is tested on each candidate (step 5 normally leaves out the primary term, which the scan has already matched).
   - `HitOrder::Kind` orders by `KindClass::label()`, then the folded extension, then name. The app's listing orders Kind by its own kind text, which the engine cannot see; results use the class instead.
   - Every `order_hits` comparison ends with the name order, then the entry number, so the sort is deterministic.
   - As built: a NUL in a query becomes `0x1A`, as in names, so a needle never holds a 0 byte. Narrowing candidates are clipped to the current range. A planned result with no hits is a valid narrowing base. `search` cannot tell whether `previous` came from the same index: the app drops `previous` whenever the index changes. Past `sorted_len()`, positions hold the right hits in no defined order until `sort_all`. Narrowing works in pieces of 2,048 candidates (16,384 gave too few pieces and was slower than a fresh scan).
   - Known disagreement, accepted: `search` ranks a symbolic link to a folder as a file (the rank key looks at the folder kind only, step 6), while `order_hits` puts it with the folders (browsable first, as listings do). HFS+ never sets `LINKS_TO_DIRECTORY`, so it shows only on the host index.
   - `query_throughput` (release, 1,000,001 entries, 27.2 MB folded, 24 cores, median of 20): rare 8-letter term 0.43 ms; common 3-letter term 1.37 ms; one letter 6.4–7.2 ms; two terms 0.84 ms; scoped (87,262 entries) 0.34 ms; filters only 0.57 ms; narrowing `phot` → `photo` 0.90 ms against 1.37 ms fresh; `sort_all` of 614,709 hits 3.0 ms. With `RAYON_NUM_THREADS=8` on the same machine the one-letter case is 8.4 ms. The full 1M/5M figures are §5's benchmark in `bench.rs`.
7. **Saved index (§6), as built.**
   - Rule order: `file_len < 8` or another magic → `None`; magic but under 80 bytes → `Truncated`; version, `id`, `change` → `None`; then the header's `file_len` against the real one, `1 <= entries <= MAX_ENTRIES`, `names_len <= u32::MAX`, the formula, all before any section is allocated. Then offsets, separators (one `memchr` pass: exactly `entries` zero bytes, each before an offset), UTF-8, flags (reserved bits `0xF0C0` zero, class at most `Font`), the root is a folder, parents. `Index::verify` runs after `from_parts` in release too, as a last check.
   - Sections are read with `take(len).read_to_end` into a buffer of the exact size (same effect as `read_exact`, gives the byte where data ran out, and does not zero 65 MB first). Allocation is bounded by the `file_len` the caller passes, so it must come from the file system's metadata, never from the file.
   - Write and read errors use `IoContext::Path { op: "writing/reading the saved search index", path: PathBuf::new() }`; the app adds the real path when it reports one. Displays with a trailing space where the empty path goes; cosmetic.
   - `index_load_time` (release, 1,000,001 entries, 65.2 MB file, warm cache): median of 5 51–61 ms, first run 88–93 ms; save 46–49 ms. Budget 200 ms. Decision 3 (no memory map) stands.
8. **`Listing` results mode (§7.2), lead decisions in the brief.**
   - `Listing` owns the background `sort_all` swap (`ensure_sorted_through(last_visible)`, on a clone of `Hits`, under the listing's generation) instead of `Search`: the order and the generation live there.
   - A navigation while in results drops them and emits `ListingEvent::ResultsDropped`; `Search` listens and resets. Activating a result is such a navigation: the search ends when the user lands in the folder.
   - The stash holds the folder's sort too; `Relevance` and `Folder` never reach folder mode, and `set_sort` refuses them there. A stash taken while the folder was still loading restores by loading again with the cursor name as focus.
   - A column sort in results survives keystrokes: each new result shows in relevance order at once and the column order replaces it when `order_hits` returns.
   - The Folder column joins names with `/` (narrow, middle-truncated column; a path reads best). Activation replaces the index's root name with the source label in the first crumb.
   - `Navigation::navigate_focus` and `Location::with_crumbs` were added for activation.
   - The cache path, the build queue and the query state are task 10.
   - As built: the list's view state is one `Shown` struct; `FolderView::{take, restore}` returns `Restored::{Shown, Resort, Reload}` (a folder re-sort still in flight when search began is re-run on leaving). `sort_all` clones, sorts and builds the order entirely on the background executor (a UI-thread clone is 60 MB at 5M hits); a cursor inside the old sorted head keeps its position without a scan, and no `CursorMoved` is emitted, so scrolling does not jump. Activating a hit in the folder already shown reloads it directly (`History::push` ignores a move to the same folder). Type-to-select in results sorted by Name binary-searches and falls back to the linear scan on a miss, because the engine's fold and the app's `compare_tokens` are not guaranteed to agree beyond ASCII.
   - Measured (`perf_entries_in_order_5m`, release, 5M hits): `entries_in_order` 7.1 ms on the UI thread in `show_results`; clone plus `sort_all` 43.8 ms in the background. Still linear on the UI thread at that size: `find_cursor` after a column sort lands, and type-to-select in results.
9. **`Search` entity (§7.1), lead decisions in the brief.** `Escape`/`leave` clears the text and the filters, so the next Ctrl+F starts clean. A query with no terms and no active filter leaves results (the folder shows) instead of showing an empty list. "This folder" resolves through `find_node` on the background executor; an origin not in the index gives scope `u32::MAX` (no hits). `Search` resets on `ResultsDropped` and on any navigation while active, so a query waiting for a building index also ends when the user moves away. Prioritizing a source moves it to the front of the queue but never pre-empts the build already running. Saves go to `<name>.idx.tmp`, then a rename. Pruning considers only names of 32 hex digits plus `.idx`, and never deletes the file just saved.
   - As built: `clear_filters` keeps `include_hidden` (a view option, not a filter); `leave` resets it too. `set_text` and `set_filters` do nothing for an unchanged value. Each build is a `BuildJob { serial, cancel, tasks }` whose drop sets the cancel; the build result and the 100 ms progress tick apply only while the job with their serial is still in the map. A build that fails shows `Failed("This volume could not be indexed (…), so it cannot be searched.")`; a cancelled one removes the state, so the next `Sources` change queues it again. The environment is read and the cache folder touched only inside the background job. `Os::MacOs` became `Os::Mac` (clippy `enum_variant_names`).
10. **Views (§7.3), split into 11a (field, top bar, keymap, wiring) and 11b (filter row, Folder column, highlights, footer, empty results), run one after the other.**
   - 11a as built: `SearchField` is an entity over a plain `LineBuffer` (char-boundary motion, tested without a window). It reads `Search`, `Listing` and `Navigation` for its placeholder and right-hand text. Text is capped at 1,024 bytes so a huge paste cannot stall line shaping; pasted `\r` and `\n` are removed. The field is a tab stop. A drag-select registers window-level move and up listeners while the button is held, so it can leave the field. `workspace::Search` (secondary-f, no context) focuses and selects the field; `workspace::LeaveSearch` (escape, `FileList` context) leaves an active search and otherwise propagates, so a later Quick Look Escape still gets the key. Typing in the field activates search when it is not active.
   - 11b lead decisions: the filter row sits inside the explorer above the column header, only while search is active. In results a header click cycles ascending, descending, back to Relevance. "Nothing matches these filters." when there are filters but no terms. `ensure_sorted_through` is called through `cx.defer`, not inside render.
   - 11b as built:
     - **Folder column truncation.** GPUI 0.2.2 has only end truncation (`TextOverflow::Truncate`), so the Folder column truncates at the end; §7.3's middle truncation waits for a GPUI that has it, or a measured truncation of our own.
     - **Size filter.** The menu uses the binary base of `format::size` (1 MB = 2^20), with adjacent, non-overlapping edges.
     - **Date filter.** The chosen date preset is remembered in `FilterRow` (a range cannot name "Last 7 days"); a missing local midnight (a DST jump) uses the first instant after it.
     - **Filter row placement.** The row sits outside the `FileList` key context and its mouse capture, so menu clicks and keys never reach the list.
     - **Menus.** Each menu takes focus when it opens and gives it back to its control when it closes. A full-window transparent backdrop under an open menu makes an outside click close it and do nothing else.
     - **Footer failure text.** The footer shows `failure_text` as given, a full sentence.
     - **Dead-code attribute.** The `expect(dead_code)` on `SortColumn::Folder` in `order.rs` was removed, since the header now constructs it.
11. **`atlas_check --index` (task 12).** After each volume's walk: `search::build`, then one line with entries, skipped, build time, entries/s, bytes per entry (`heap_bytes / len`) and total heap. A skipped count above zero counts as a problem in the exit code. Run on `fuzz/seeds/hfsplus_mount/plain`: 3 entries, 0 skipped. Not yet run on a real disk.
