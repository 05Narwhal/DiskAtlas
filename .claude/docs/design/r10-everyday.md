# R10 to R18. Menus, properties, settings, folder sizes, the storage bar and Mac housekeeping files

Written by Fable on 2026-11-13, from Julian's second brief of that day, after reading `r1-redesign.md` and the code it stands on. **Status: approved by Julian on 2026-11-13 (decisions 29 to 42, §11). Build it in the order of §10.** It adds nine milestones to the fourteen of R1, changes seven things in R1 (§1), and gives one build order for all of them (§10). Fable read it a second time against the code the same day; what that changed is in §13.

Julian asked for: right-click menus that work, a properties window, Mac housekeeping files hidden on every system and put to use, a designed settings section, a disk bar people can read (partitions, then what kinds of files fill them), and folder sizes that are computed in the background and stay right when files change.

Mockups for this file (four boards: storage bar and folder sizes, right-click menus, Properties, Settings; each with a Day/Night switch in Tweaks): https://claude.ai/artifact/St9vwbp8Ber5eQuB79CekT. They show the layout and the wording. Kind glyphs are stand-in shapes there; the real ones come from R2.

## 0. The idea that holds it together

One background measurement feeds five things. Every volume gets a **size table**: for each folder, the bytes and files inside it, and for the volume, the bytes per kind. The Size column, the storage bar, the space map (R7), the folder preview (Q1) and the properties window all read that one table. Nothing else in this file computes a size.

- On a volume Disk Atlas reads itself, the table comes from the search index in one pass (§3.1). The index already holds what is needed: verified below.
- On the host, the table comes from a walker that measures the folders on screen first and a watcher that corrects it when files change (§3.2 and §3.3).

## 1. Review of R1: what changes

Checked against the code on 2026-11-13.

| # | R1 says | Found | Change |
|---|---|---|---|
| 1 | §3.5: verify whether `Index` keeps each entry's parent and size; if not, stop and ask | It keeps both: `parent: Box<[u32]>` and `size: Box<[u64]>` (`search/index.rs:233`, `:236`), in depth-first pre-order with `parent[i] < i`. A folder's total is one reverse pass | Not a stop-and-ask. R7 reads `IndexSizes` (§3.1) and waits for R13 |
| 2 | §3.3: the strip shows occupancy as relief, with scale ticks | Julian: unreadable for most people | Replaced by the storage bar (§4). The relief and the file's pieces move to the disk view as a second mode, "Layout" |
| 3 | §1.2 lists twelve kinds; §1.7 says kind comes from extension, then type code | Three tables disagree: `search::kind::KindClass` (eleven classes, extension only, no 3D model), `format::KINDS` (labels by extension), `details::type_code_kind` (labels by type code). On a classic Mac disk most files have no extension, so the index calls them all Other and a bar by kind would be one grey block | One classifier in the engine (§2), fed the type code during the scan |
| 4 | §9 has no milestone for the settings file, but §1.4 (density), §2.3 (motion) and §6 (window bounds) all store settings | `m8-formats-plugins.md` §8.1 builds settings in M8, after all of R | The settings store is pulled forward as R10a, the window as R10b (§8) |
| 5 | §4: "Watching folders for outside changes is not in R8" | Folder sizes that stay right need a watcher anyway | The watcher (R14) also refreshes the open folder's rows |
| 6 | §5 and §6 build Quick Look's window chrome inside `views/preview.rs` | Properties and Settings need the same chrome | R5 puts it in `views/aux_window.rs` (title bar, close, drag area, remembered bounds) and Quick Look uses it |
| 7 | The filter menus are a private menu in `components/toolbar.rs:561` | Context menus need the same thing with submenus, shortcuts and separators | One `components/menu.rs` (§6.1); the filter menus move onto it |

Everything else in R1 stands as written.

## 2. Kinds: one classifier

`crates/lib/src/search/kind.rs` becomes the only place that decides a file's kind class.

```rust
// KindClass gains one variant. Values 0..=10 keep their meaning.
pub enum KindClass { Other = 0, Folder, Document, Image, Audio, Video, Archive,
                     Application, Code, DiskImage, Font, Model /* = 11 */ }

/// The class of a file: by extension; when that gives `Other` and a type
/// code is known, by type code. Never allocates.
pub fn classify(name: &str, finder_type: Option<[u8; 4]>) -> KindClass;
```

- Labels follow R1 §1.2: `Image` reads "Picture", `Video` "Movie", `Code` "Code and data", `Model` "3D model". Extensions for `Model`: `stl obj ply off gltf glb 3mf usdz fbx step stp iges igs blend`.
- Type-code table (sorted, static, the same lookup as extensions): `TEXT ttro utxt` Document; `PDF ` Document; `APPL appe cdev INIT` Application; `PICT JPEG GIFf PNGf TIFF 8BPS PNTG BMPf` Image; `MooV MPEG MPG ` Video; `AIFF AIFC WAVE sfil MPG3 Mp3 MiDi` Audio; `ZIP SIT! SITD SIT5 Gzip BINA` Archive (`BINA` only with creator unknown, so leave it out if in doubt); `FFIL tfil sfnt LWFN` Font; `dimg dImg rohd devi` DiskImage. The lead may add codes; each addition gets a test line.
- `ScanEntry` (`fs/mod.rs:567`) gains `pub finder_type: Option<[u8; 4]>`. It is not `#[non_exhaustive]`, so every place that builds one adds the field: the HFS+ scan fills it from the catalog record it already holds (`FInfo.fdType`), everything else passes `None`.
- `EntryFlags` (`fs/mod.rs:63`, bits 0 to 2 used) gains `PACKAGE = 1 << 3` (Finder bundle bit), `ALIAS = 1 << 4` (Finder alias bit), and the Finder label in bits 8 to 10 with `pub const fn label(self) -> u8` (0 = none). The HFS+ listing and scan set them from the Finder flags they already read.
- `IndexFlags` (`search/index.rs:80`): bit 6 = a housekeeping item (§5.1) **or anything inside one**, bit 7 = package (the package's own entry only), bits 12 to 14 = label. Bits 8 to 11 already hold the class and fit `Model`. The index build passes bit 6 down from a folder to everything in it, so search and sizes test one bit and never walk up. The names come from `mac::conventions::system_item`, which is why that table is built in R12 and not R16 (§10): an index saved as version 2 must already carry the bit.
- `search/persist.rs:100`: `VERSION` goes from 1 to 2. A saved index of version 1 is refused by the existing check and rebuilt once. This is Fable's call under README rule 4: no array is added or resized, only reserved bits get meanings.
- The app's label tables stay (`format::kind`, `details::kind_text`: "JPEG image" is finer than "Picture"), but glyph, tint, filter and every byte count use `classify`.

## 3. Folder sizes

### 3.1 Volumes Disk Atlas reads: `IndexSizes` (`crates/lib/src/sizes/index.rs`, new)

```rust
/// What is inside one folder, everything below it included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Data forks plus resource forks, in bytes. Saturating.
    pub bytes: u64,
    /// Files and links.
    pub files: u64,
    /// Folders inside, this one not counted.
    pub folders: u64,
}

/// Bytes per kind class, indexed by `KindClass as usize`, plus housekeeping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassBytes { pub by_class: [u64; 16], pub system: u64 }

pub struct IndexSizes { /* Per folder, in entry order: entry: Box<[u32]>, bytes: Box<[u64]>,
                           files: Box<[u32]>, folders: Box<[u32]>, end: Box<[u32]>;
                           by_node: Box<[(u64, u32)]>, volume: ClassBytes, flags */ }

impl IndexSizes {
    /// A few passes over `index`. `None` when `cancel` was set (checked
    /// every 65,536 entries).
    pub fn build(index: &Index, cancel: &AtomicBool) -> Option<Self>;
    /// `None` when `entry` is not a folder. O(log folders): the app
    /// resolves a row once and keeps the answer (§3.4).
    pub fn totals(&self, entry: u32) -> Option<Totals>;
    /// The entry number of a folder, by its node: the entry that holds the
    /// children when a folder was reported more than once. Binary search.
    pub fn folder_entry(&self, node: NodeId) -> Option<u32>;
    /// The direct children of a folder, in index order. O(children).
    pub fn children(&self, entry: u32) -> impl Iterator<Item = u32> + '_;
    /// Bytes per kind for the whole volume. O(1).
    pub fn volume_classes(&self) -> ClassBytes;
    /// Bytes per kind inside one folder. O(subtree): call off the UI thread.
    pub fn classes_of(&self, index: &Index, entry: u32) -> ClassBytes;
    /// True when some sizes were clamped or the scan skipped structures.
    pub fn is_lower_bound(&self) -> bool;
    pub fn heap_bytes(&self) -> usize;
}
```

**What the two numbers mean.** They differ on purpose, and hard links are why.

- **A folder's total is what copying it out would give you.** Every name inside counts: a file with three names in the folder counts three times, and a folder reached through a directory hard link (every Time Machine backup is made of them) counts its target's contents. This is the same rule as on the host (§3.2), so one sentence in Properties covers both: "Files with several names are counted once for each."
- **The volume's bytes per kind count each file once**, so the storage bar never claims more than the disk holds.

**How hard links arrive** (read on 2026-11-13). The HFS+ scan reports a link as an entry with the target's node, kind and sizes (`fs/hfsplus/scan.rs:351` to `:383`), and also reports the target itself where it really is: `iNode<n>` in `HFS+ Private Data`, `dir_<n>` in `.HFS+ Private Directory Data`. For a folder reported more than once the index hangs the children from the first report (`search/build.rs:23`, tests at `:685` and `:709`), and the catalog pass reports `dir_<n>` before any link. So in the index **every visible directory link is a folder with no children**, and its contents sit under the private store. A size table that ignored this would show every folder of a Time Machine disk as empty.

Algorithm, all with `saturating_add` and `get()` (the arrays come from a disk). A scratch array of 4 bytes per entry (entry to folder rank) lives only during the build.

1. **Private stores.** Among the root's direct children, those that `mac::conventions::system_item` classes `HfsPrivate` (two at most). Their entry ranges are kept. On any other format there are none and steps 3 and 5 change nothing.
2. **Reverse pass for `end`**, `i` from `n − 1` down to 1: `end[i]` starts at `i + 1`; `end[parent[i]] = max(end[parent[i]], end[i])`. Because `parent[i] < i`, one pass is bottom-up.
3. **Forward pass, twice: first the entries outside the private stores, then those inside.** For each file, own bytes = `size` (0 when `NO_SIZE`) + `rsize` (0 when `NO_RSIZE`); they go to its folder's own total every time. For `volume`, a file whose node was already seen adds nothing; "seen" is a bit set over node numbers when the largest node is below `DENSE_NODE_LIMIT = 1 << 28` (32 MiB of bits at most), else a sorted, de-duplicated copy of the file nodes. An unseen file's bytes go to `system` when flag bit 6 is set, else to the class of the package it is inside (flag bit 7 on a folder, until the entry number reaches that folder's `end`; the outermost package wins), else to its own class. Going outside first means a hard-linked file is classed by a name the user gave it, not by `iNode4127`. A file directly inside a private store that no link claimed is an orphan and counts as `system`.
4. **Reverse pass for totals**: `bytes`, `files` and `folders` of each entry are added to its parent. A private store is not added to the root (its bytes are already counted under the names that link to them).
5. **Directory links**, only when some folder node has more than one entry. For such a node the *target* is the entry that has children (if none has, all are empty and nothing is done); every other entry is a *link*. `L(target)` = the target's total from step 4 plus `L` of the target of every link inside its range, worked out with a memo per target, a depth limit of `MAX_LINK_NESTING = 64` and an "in progress" mark, so a damaged disk where a folder links into itself ends at 0 and sets the lower-bound flag. Each link's own total is set to `L` of its target, and step 4 is run once more with those in place. Cost: one more pass plus one binary search per link.
6. **`by_node`**: (node, entry) for every folder node, sorted by node; for a node with several entries, the target.

If the sum of `volume` is larger than `VolumeInfo::total_bytes`, an `overcommitted` flag is set (a damaged catalog can claim anything): the bar then scales by the sum, and Properties says "The files on this volume claim more space than it has." A folder's total may be larger than the volume without any damage (hard links); that sets nothing.

Cost: 24 bytes per folder plus 12 per folder node, nothing per file: about 4 MB for 1,000,000 entries of which 100,000 are folders, on top of the index's 80 MB. Sizes are logical bytes. The index does not hold allocated size, so "on disk" is shown for single files only (from `extents`), never for folders.

**When it runs.** Today the index is built when search is first used (`state/search.rs:192`, `:369`; verify). From R13 the build starts when a volume is opened, at background priority, behind the first folder listing, and `IndexSizes::build` runs when it finishes or when a saved index loads. Setting `index_on_open` (default on) turns this off; then folders show no size until the user searches or picks "Calculate sizes".

**Changes.** A volume Disk Atlas reads is read-only and does not change under it. When the window becomes active again, the app compares `VolumeIdentity::change` (and for an image, the image file's length and modified time) with what the index was built from; on a difference it reloads the listing and rebuilds the index and the sizes. F5 does the same on request.

### 3.2 The host: `HostSizes` (`crates/lib/src/sizes/host.rs`, new)

The host's index is built only for search, with scan-local ids, and never saved. Folder sizes on the host do not use it. They use a tree of folders that is filled lazily and corrected in place.

```rust
pub enum SizeState {
    /// Not measured and not queued.
    Unknown,
    /// Being measured: the total so far.
    Running(Totals),
    /// Measured, complete.
    Exact(Totals),
    /// Measured, but some folders inside could not be read.
    AtLeast(Totals),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority { Background, Listed, Visible }

/// A folder in the tree: its slot and the slot's generation, so a handle
/// to a folder that was deleted (and whose slot was reused) answers `Unknown`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostDir { slot: u32, generation: u32 }

pub struct HostSizes { /* arena of folders, queues, workers */ }

impl HostSizes {
    /// One per host drive or place. `on_change` is called from a worker
    /// thread, at most once per `NOTIFY_INTERVAL`, when any total changed.
    pub fn new(root: PathBuf, on_change: Box<dyn Fn() + Send + Sync>) -> Self;
    /// Queue `dir` and everything inside it. Raising a priority re-queues.
    pub fn request(&self, dir: &Path, priority: Priority);
    /// Drop every queued folder below `Listed` priority that is not inside
    /// one of `keep`. What was measured stays.
    pub fn refocus(&self, keep: &[PathBuf]);
    pub fn state(&self, dir: &Path) -> SizeState;
    pub fn classes(&self, dir: &Path) -> ClassBytes;
    /// The files directly in `dir`, its sub-folders not counted.
    pub fn own(&self, dir: &Path) -> Totals;
    /// The sub-folders of `dir` that are known, with their states. For the
    /// space map (R7) and "What is inside" (§7). Off the UI thread.
    pub fn children(&self, dir: &Path) -> Vec<(OsString, SizeState)>;
    /// Is `dir` in the tree? Cheap; the watcher asks it for every event.
    pub fn knows(&self, dir: &Path) -> bool;
    /// A handle a row keeps, so a frame never resolves a path.
    pub fn dir(&self, dir: &Path) -> Option<HostDir>;
    /// O(1). `Unknown` when the folder behind the handle is gone.
    pub fn state_of(&self, dir: HostDir) -> SizeState;
    /// Bytes measured so far under the root, for the bar's percentage.
    pub fn measured_bytes(&self) -> u64;
    pub fn heap_bytes(&self) -> usize;
    /// The direct entries of `dir` changed: read it again and pass the
    /// difference up. Unknown folders are ignored.
    pub fn invalidate(&self, dir: &Path);
    /// Everything may have changed (the watcher lost events).
    pub fn invalidate_all(&self);
    /// Stop the workers and wait for them.
    pub fn shutdown(&self);
}
```

- **Arena.** One node per folder met: parent, name, own bytes and files (direct children only), own bytes per class (only the non-zero classes), subtree `Totals`, state, children sorted by name (so a path resolves in depth × log fan-out). Files get no node. At most `MAX_HOST_FOLDERS = 2_000_000` nodes per tree; past that the walk stops and the roots affected become `AtLeast`. **Memory is a budget** (§3.6): names live in one shared byte buffer (offset and length, not one allocation each), children are a boxed slice of node numbers, and the per-class bytes sit in a side table that most folders (one or two kinds) share a small entry of. Target: 96 bytes per folder or less, checked by a test through `heap_bytes`.
- **Work unit: one folder.** `std::fs::read_dir`, and for each entry `DirEntry::file_type` and `DirEntry::metadata` (on Windows both come from the directory read, no extra system call; on Unix one `lstat`). Files add `len` to the folder's own bytes and to their class by `classify(name, None)`. A folder becomes a child node and a queued unit. Never followed: anything `FileType::is_symlink` reports (symbolic links, and on Windows junctions), and on Unix a folder whose `dev` differs from the root's (other mounts, `/proc`, `/sys`). A folder that cannot be read marks itself and every ancestor `AtLeast`. Files are never opened, so a cloud placeholder is counted at its logical size and never downloaded. (Listing a cloud folder that was never opened may make the sync program fetch that folder's file list, as Explorer does; no file content is fetched.)
- **A renamed or moved folder** reaches the tree as one folder gone and one new: its subtree is measured again. That is the price of not opening every folder to read a file id; it costs one walk of that subtree in the background, and the old name's numbers are dropped at once.
- **Workers.** `SIZE_WORKERS = clamp(cores / 2, 1, 4)` threads owned by `HostSizes` ("disk-atlas-sizes"), parked when idle, started on the first request, joined in `shutdown` and on drop. They take the highest priority first and, within a priority, the most recently queued folder (depth-first, so one visible folder finishes before the next starts).
- **Totals go up as they are found.** Each finished unit adds its own bytes to every ancestor (depth ≤ `search::index::MAX_DEPTH`, 4096). A subtree becomes `Exact` when its last queued unit finishes.
- **Hard links** on the host are counted once per name. Said in Properties: "Files with several names are counted once for each."
- **What is measured.** Setting `folder_sizes`: `open_folders` (default) measures the folders in the folder you are looking at, visible rows first, and keeps going in the background while you stay; leaving calls `refocus`, which drops the unfinished background work of the folder you left and keeps what was measured. `whole_drives` never drops work: opening a drive measures all of it. `off` measures nothing; a folder's menu has "Calculate size".
- **Network and removable drives** are not measured unless `folder_sizes_network` is on (default off); their folders show no size and offer "Calculate size". Read on 2026-11-13: the app does not know a host drive's type today (`state/sources.rs:157` has `removable` for physical disks only, and nothing calls `GetDriveTypeW`). It can be told without new `unsafe`:
  - *Network.* Windows: `std::fs::canonicalize` of the drive's root begins `\\?\UNC\` for a mapped network drive, and a path that itself begins `\\` is one (verify first with a mapped drive). Linux: the filesystem type in `/proc/self/mountinfo` is one of `nfs nfs4 cifs smb3 9p afs ceph` or begins `fuse.` (`fuse.sshfs`, `fuse.rclone`). macOS: the `mount` command's line for the volume names `nfs`, `smbfs`, `afpfs` or `webdav`; run once when `/Volumes` changes, off the UI thread.
  - *Removable.* From the drive-to-disk mapping R3 builds (R1 §3.2) and that disk's `removable`. So R14 comes after R3, which it does.
  - A drive whose type cannot be told counts as network: not measured until asked.
- **Later, not in this plan:** once the device helper (R9) and an NTFS reader exist, a host NTFS drive could be measured from its file table in seconds, read-only. Noted so nobody tries it early.

### 3.3 Watching (`crates/lib/src/sizes/watch.rs`, new; needs the `notify` crate, decision 29)

```rust
pub struct Watcher { /* notify::RecommendedWatcher, a channel, a flush thread */ }

impl Watcher {
    /// Watch `root` and everything inside it. `wanted` is asked for the
    /// folder of each event, on the watcher's thread, and must be cheap;
    /// a folder it refuses is dropped at once. `sink` receives batches.
    pub fn new(
        root: &Path,
        wanted: Box<dyn Fn(&Path) -> bool + Send + Sync>,
        sink: Box<dyn Fn(WatchBatch) + Send + Sync>,
    ) -> Result<Self>;
}

pub enum WatchBatch {
    /// The direct entries of these folders changed. De-duplicated,
    /// at most `MAX_BATCH_DIRS`.
    Dirs(Vec<PathBuf>),
    /// Events were lost: treat everything as changed.
    Rescan,
}
```

- **Most events are thrown away, on purpose.** A system drive changes all day in folders nobody measured (temporary files, logs, browser caches). The app's `wanted` is "`HostSizes::knows` this folder, or it is the folder a window is listing". Everything else is dropped before it is stored, so background noise cannot fill a batch and force a full re-measure. A new folder is still caught: its event names the folder that holds it, which is known.
- Events are reduced to "the folder that holds the changed entry", collected in a set, and flushed every `WATCH_FLUSH = 500 ms`. A flush with more than `MAX_BATCH_DIRS = 4096` folders becomes `Rescan`. `notify`'s rescan flag (Windows: the change buffer overflowed) also becomes `Rescan`.
- The app passes `Dirs` to `HostSizes::invalidate` (one `read_dir` per folder, the difference added to every ancestor) and `Rescan` to `invalidate_all` (the tree is measured again in the background; old numbers stay on screen until the new ones replace them).
- **What a flush may cost (decided by Fable, 2026-11-14, after R14.2's measurement; §14).** `invalidate` stays as it is: it reads the folder on the thread that calls it, one folder after another, and the app calls it from one background thread. The budget is per folder: **200 µs or less for each changed folder, so 200 ms or less for a flush of 1,000**, on every platform, and nothing on the UI thread. The first figure (50 ms) was written without a measurement and is below what the platform allows: on NTFS a bare `read_dir` and `metadata` loop over 1,000 small folders takes 70 to 104 ms. What the budget protects is that applying a flush stays well inside `WATCH_FLUSH`, so sizes follow a change within about a second: at 200 µs one thread absorbs 2,500 changed folders per flush. Spreading a batch over the size workers was the other choice and is not taken: it would put the watcher's work in the same queues as the walk, in the file where a mistake costs most, to save about 100 ms in the background during a burst, and it could not hold on a machine with one size worker (`SIZE_WORKERS` is 1 below four cores). Look at it again only if a trace in R14.3 shows the open folder's size more than 1 s behind a change.
- **Batches never pile up (R14.3, app side).** The thread that applies batches takes every batch waiting on the channel at once and merges them into one set, the folder a window is listing first. If the set holds more than `MAX_BATCH_DIRS` folders, or any batch is `Rescan`, it calls `invalidate_all` once and reads no folder itself. So the work for one pass is bounded at 4,096 folder reads (about 0.8 s at the budget), and a long burst (a build, a checkout, an unpack) ends in one background re-measure and not in a growing queue.
- Windows and macOS watch a whole tree with one handle. Linux needs one watch per folder: `notify` adds them, and Disk Atlas asks only for the folders in the arena nearest the open folder, up to `MAX_LINUX_WATCHES = 4096`. Folders past that are measured again when the user comes back to them, if their numbers are older than `REVALIDATE_AFTER = 60 s`.
- One watcher per host drive that has an open window on it or measured folders, created on first use, dropped when neither holds.
- **The open folder's rows** use the same batches: when the listed folder is in a batch, `Listing` reads it again after `LIST_REFRESH = 200 ms` and merges by name. Selection, cursor and scroll position stay on the same names; a selected row that vanished is dropped from the selection. No animation.
- Setting `watch_changes` (default on). Off: sizes and rows change only on F5 and on coming back to a folder.

### 3.4 In the app (`crates/app/src/state/sizes.rs`, new)

```rust
pub enum Table {
    Index(Arc<IndexSizes>),
    Host { sizes: Arc<HostSizes>, watcher: Option<Watcher> },
}

/// What a folder row keeps. Resolved once, when the listing loads or refreshes.
#[derive(Clone, Copy)]
pub enum RowSize {
    /// A file, or a place where sizes are off or not built yet.
    None,
    /// A volume Disk Atlas reads: final.
    Fixed(Totals, /* lower bound */ bool),
    /// The host: read through the handle when the row is drawn.
    Live(SourceId, HostDir),
}

pub struct Sizes { /* tables: HashMap<SourceId, Table>, a channel, the repaint timer */ }

impl Sizes {
    pub fn new(cx: &mut Context<Self>) -> Self;
    /// From `Search`, when an index was built or loaded: builds the table
    /// on the background executor and notifies when it is in place.
    pub fn index_ready(&mut self, source: SourceId, index: Arc<Index>, cx: &mut Context<Self>);
    /// From `Listing`, when a host folder is shown: creates the tree and the
    /// watcher on first use, requests the folder (`Listed`) and calls
    /// `refocus` for the one that was left. Follows the `folder_sizes` settings.
    pub fn folder_shown(&mut self, source: SourceId, dir: &Path, cx: &mut Context<Self>);
    /// From the list, after scrolling stops for 60 ms: these rows are `Visible`.
    pub fn rows_visible(&mut self, source: SourceId, dirs: &[PathBuf]);
    /// Background executor only (it may search or lock).
    pub fn resolve(table: &Table, source: SourceId, parent: &Path, node: NodeId, name: &str) -> RowSize;
    /// O(1), for drawing.
    pub fn state(&self, row: RowSize) -> SizeState;
    /// The volume's kinds, and how much of the used space they account for (0.0 to 1.0).
    pub fn classes(&self, source: SourceId) -> Option<(ClassBytes, f32)>;
    pub fn table(&self, source: SourceId) -> Option<&Table>;
    /// The source went away: drop its table, stop its watcher and workers.
    pub fn source_closed(&mut self, source: SourceId);
}
```

- `Sizes` entity: per source, either `Arc<IndexSizes>` or `Arc<HostSizes>` with its `Watcher`. `on_change` and the watcher sink send on a channel; one foreground task drains it and calls `cx.notify()` at most every `SIZE_REPAINT = 250 ms`. `Listing`, the storage bar, Properties and the space map observe `Sizes`; none of them holds a table of its own.
- **A row asks in O(1).** When a listing loads or refreshes, each folder row is resolved once. On a volume Disk Atlas reads, the table never changes, so the row simply keeps its `Totals` (`folder_entry(node)`, then `totals`). On the host the row keeps the arena node (resolved from the listed folder's node and the row's name) and rendering reads through it. No path is resolved and nothing is searched during a frame.
- **The Size column** for a folder: `Exact` reads like a file's size. `Running` shows the total so far in `ink3` with "…" after it. `AtLeast` shows "over 3.1 GB" with the tooltip "2 folders inside could not be read." `Unknown` shows nothing. The status bar's item line adds the selection's total.
- **Sorting by size** uses folder totals. While any are `Running`, the list re-sorts at most every `RESORT_INTERVAL = 1 s`, never while a mouse button is down, never within 500 ms of a key press, and the cursor row stays where it is on screen.
- **Packages** (§5.4) show their total like a file.

### 3.5 When things fail

| What happens | What the user gets |
|---|---|
| The index build was cancelled, or the scan skipped damaged structures (`ScanSummary` says so) | The table is built from what there is, flagged lower bound: every folder total reads "over 3.1 GB", the bar's label reads "at least 252.7 GB counted", and Properties says "Part of this volume could not be read, so sizes are a lower bound." |
| The index build failed outright | No sizes on that volume. The bar shows used and free from `VolumeInfo`; the Size column is empty for folders; the volume's menu offers "Rebuild the index" |
| `index_on_open` is off and nothing was searched yet | The same picture, with a text button in the bar: "Calculate sizes" |
| The watcher cannot start (Linux watch limit reached, a network share, a drive that does not report changes) | Sizes are still measured. A folder is measured again when it is shown, if its numbers are older than `REVALIDATE_AFTER`. The status bar's size item gets the tooltip "Changes on this drive are not watched. F5 refreshes." Logged once per drive, not per folder |
| A host folder cannot be read (no permission) | It and its ancestors read "over …"; the tooltip counts the unreadable folders |
| The tree reaches `MAX_HOST_FOLDERS` | The walk stops; the roots affected read "over …"; Properties of the drive says "This drive has more folders than Disk Atlas measures at once." |
| A size worker panics on one folder | The unit is wrapped in `catch_unwind`: that folder becomes `AtLeast`, the worker goes on, the panic is logged |
| The disk is disconnected | The table is kept and drawn at 45 % with the listing (R1 §1.8), and dropped when the source closes |

### 3.6 Budgets (release build, measured the M3 way, recorded in `performance.md`)

| What | Budget |
|---|---|
| `IndexSizes::build`, 1,000,000 entries | ≤ 150 ms (R1 §3.5's budget, now with an owner); with 100,000 directory links among them, ≤ 250 ms |
| `HostSizes` memory, 1,000,000 folders | ≤ 96 MiB (`heap_bytes`) |
| `classes_of` on a 100,000-entry folder | ≤ 2 ms |
| Host walk, warm cache, NVMe, 500,000 files | ≤ 3 s to `Exact` |
| Size cells in a frame of 60 visible rows | ≤ 0.2 ms; no frame over 16.7 ms while sizes arrive |
| One watcher flush of 1,000 changed folders | ≤ 200 ms on one background thread (200 µs per folder), 0 on the UI thread. Restated 2026-11-14 from 50 ms, see §3.3 |
| Idle cost with a watcher on a busy system drive | < 1 % of one core, averaged over a minute |

## 4. The storage bar (`components/storage_bar.rs`; replaces the drawing in `components/disk_strip.rs`)

What a person asks of this bar: which partitions are on this disk, how full is each, and what is filling the one I am in. It answers in that order.

```
Samsung T7 · 931.5 GB · read-only                 ● Iceland 2014 · 18.2 GB · 7 % of Macintosh HD
[ Pictures | Movies   |Aud|Docs|Apps|Oth|Sys| ≈≈≈ free ≈≈≈ ][ ▒▒▒▒▒▒▒ ≈≈≈≈ ][▨▨]
 └────┘ (the selection's measure, in pin)
 Macintosh HD                                              Time Machine     EFI
 252.7 GB used of 465.1 GB                                 310 GB of 465 GB
 ■ Pictures 96.4 GB  ■ Movies 71.0 GB  ■ Audio 22.1 GB  ■ Documents 14.8 GB  ■ Applications 12.9 GB  …
```

- **Partitions to scale**, 20 px tall, radius 6, 2 px of `sheet` between them. A partition is never narrower than `MIN_PARTITION_PX = 28`; when that changes the scale, the others share what is left and the narrow one's tooltip says its real size. `disk_strip::layout` (`:80`) already does the to-scale part.
- **Inside the current partition:** one segment per kind, in a fixed order that never changes (Pictures, Movies, Audio, Documents, Applications, Code and data, Archives, Disk images, 3D models, Fonts, Other, System), each filled with its kind tint at full strength (R1 §1.2), 1 px of `sheet` between segments, then free space as hatched water. A kind under `MIN_SEGMENT_PX = 3` joins Other. "System" is the housekeeping files of §5 plus what the volume uses that no file accounts for (catalog, journal, slack), drawn in `relief` at 35 %. The order is fixed so the bar does not reshuffle while it is being measured and so two disks can be compared.
- **Other partitions:** the same segments at 55 % when their sizes are known; otherwise used space in `relief` at 30 % and free as water, from `VolumeInfo`. A partition Disk Atlas cannot read is hatched in `line` and labelled with its format. Unallocated space is a dashed outline.
- **Drives of this computer.** A host drive's kinds come from `HostSizes::classes` of its root, which is complete only when the whole drive was measured, and by default it is not (`folder_sizes` is "folders I open"). So a host drive's bar starts with used and free from the system (`relief` at 30 %, then water) and a text button at the right of the label line: "See what fills this drive". The button requests the drive's root at `Listed` priority for this session, and the bar then fills as under "While measuring". With `whole_drives` it starts by itself. Host totals count a file once for each name and count logical bytes, so their sum can pass what the system calls used (hard links in `WinSxS`, compressed and sparse files): the segments are then scaled to the used bytes and the legend's numbers read "about 96 GB".
- **Labels under each partition:** its name, then "252.7 GB used of 465.1 GB" in 12 px `ink2`. A partition too narrow for its label keeps it in the tooltip.
- **Legend** (current volume only): one chip per drawn kind, in the same order: tint square, glyph, name, bytes. Two lines at most; the rest fold into "and 3 more". Hovering a chip or a segment dims the others to 40 %. Clicking either searches this volume for that kind, largest first (the M4 Kind filter and Size sort).
- **While measuring, on this computer:** the part of the used space not yet accounted for is `track` with the R1 sweep, and the right end of the label line reads "Measuring… 61 %". The percentage is bytes accounted for ÷ used bytes (`HostSizes::measured_bytes`), so it is real. Segment widths ease over `STANDARD`; legend numbers change at most four times a second.
- **While measuring, on a disk Disk Atlas reads:** kinds arrive all at once, because the table is made in one step after the file list is read. Until then the used part is `track` with the sweep and the label reads "Reading the file list… 18,400 folders" (the counter the index build already keeps, `state/search.rs:384`). Then the segments grow in from the left over `EMPHASIZED`, once.

**Layout is a pure function**, tested without a window:

```rust
// components/storage_bar.rs
pub struct BarPartition {
    pub bytes: u64,
    pub current: bool,
    pub fill: Fill,
}
pub enum Fill {
    /// Kinds are known. `accounted` ≤ `used`: the rest is drawn as not yet measured.
    Kinds { classes: ClassBytes, used: u64, accounted: u64 },
    /// Only the system's numbers.
    UsedFree { used: u64 },
    /// No reader for this format, or it failed to mount.
    Unreadable,
    Unallocated,
}
#[derive(Debug, PartialEq)]
pub enum Piece { Kind(KindClass), System, NotMeasured, Used, Free, Hatched, Outline }
#[derive(Debug, PartialEq)]
pub struct Rect1 { pub x: f32, pub w: f32, pub piece: Piece }
pub struct Laid { pub x: f32, pub w: f32, pub pieces: Vec<Rect1> }

/// `width` in px. Partitions to scale with `MIN_PARTITION_PX` and 2 px gaps;
/// inside, the fixed kind order, `MIN_SEGMENT_PX`, 1 px gaps. Never allocates
/// more than one `Vec` per partition. Widths are whole pixels and sum to `width`.
pub fn layout(partitions: &[BarPartition], width: f32) -> Vec<Laid>;
/// The legend's chips, in the bar's order: the kinds that got a segment,
/// with the folded ones summed into `Other`.
pub fn legend(classes: &ClassBytes, used: u64, bar_width: f32) -> Vec<(Piece, u64)>;
```

- **Keyboard and screen readers.** The bar is one tab stop. Left and Right move between partitions and, inside the current one, between segments; Enter does what a click does; the Menu key opens the item's menu. Each stop has a name in words ("Macintosh HD, 252.7 GB used of 465.1 GB", "Pictures, 96.4 GB, 38 percent of what is used").
- **The selection's measure:** a `pin` bracket under the bar, as wide as the selection's bytes at the bar's scale (2 px at least), starting at the left edge of that kind's segment for a file, or of the partition for a folder or several items. Its text sits at the right of the first line with a `pin` dot. This replaces R1's piece marks here: the bar shows amounts, not positions.
- **Layout mode** (disk view only, R1 §3.4): a "Contents | Layout" switch over the large bar. Layout is R1 §3.3's drawing, unchanged: occupancy relief, scale ticks, the selected file's pieces in `pin`. `state/strip.rs` keeps serving it.
- **Sidebar disk rows** keep a 4 px bar: partitions to scale, used part `ink3`, free `track`. No kinds at that size.
- Setting `storage_bar`: `full` (default), `compact` (no legend), `hidden`.
- Right-click: on a partition, "Open" and "Properties"; on a segment or chip, "Show the largest Pictures" and "Show on the space map".
- Every segment and chip has a tooltip in words ("Pictures: 96.4 GB, 38 % of what is used"). Kind is never colour alone: the chip carries the glyph and the name.

## 5. Hidden items, housekeeping files, and what Disk Atlas does with them

### 5.1 Two switches, not one

| Setting | Default | Governs |
|---|---|---|
| `show_hidden` (Ctrl+Shift+.) | off | What the volume itself marks hidden: a leading dot, the Finder's invisible flag, the Windows hidden attribute (`EntryFlags::HIDDEN`, as today) |
| `show_system_items` | off | The housekeeping items in the table below, on every filesystem and on the host, on every operating system |

The two are independent: with only `show_hidden` on, `.gitignore` appears and `.DS_Store` does not. A housekeeping item that is shown is drawn at 55 % and its Kind column says what it is ("Finder view settings").

```rust
// crates/lib/src/mac/conventions.rs (new). Pure: no I/O.
pub enum SystemItem { DsStore, AppleDouble, FolderIcon, Localized, MacOsxArchiveFolder,
    SpotlightStore, FsEvents, Trash, TemporaryItems, DocumentRevisions, VolumeIcon,
    HiddenList, IndexingMarker, TimeMachineMarker, AfpShare, HfsPrivate, HfsJournal,
    ClassicDesktopDb, ClassicDesktopFolder, ClassicHousekeeping,
    WindowsThumbs, WindowsDesktopIni, WindowsRecycleBin, WindowsVolumeInfo,
    LinuxLostFound, LinuxTrash }

/// `hidden`: the entry already carries `EntryFlags::HIDDEN`.
pub fn system_item(name: &str, kind: NodeKind, at_root: bool, hidden: bool) -> Option<SystemItem>;
impl SystemItem { pub const fn label(self) -> &'static str; pub const fn explains(self) -> &'static str; }
```

Names match exactly (ASCII case-insensitive for the Windows rows only). The index build calls it, sets flag bit 6 on the item and passes the bit down to everything inside it (§2), so search needs no names and a file deep inside `.Spotlight-V100` never turns up as a search hit or as a kind in the storage bar. `IndexSizes` calls it only for the root's direct children, to find the HFS+ private stores (§3.1).

| Where | Names | Class |
|---|---|---|
| Anywhere | `.DS_Store` | DsStore |
| Anywhere, files | `._` followed by at least one character | AppleDouble |
| Anywhere | `Icon\r` | FolderIcon |
| Anywhere | `.localized` | Localized |
| Anywhere, folders | `__MACOSX` | MacOsxArchiveFolder |
| Root | `.Spotlight-V100`, `.fseventsd`, `.TemporaryItems`, `.DocumentRevisions-V100`, `.PKInstallSandboxManager`, `.PKInstallSandboxManager-SystemSoftware`, `.MobileBackups`, `.vol` | their classes |
| Root | `.Trashes`, `.Trash` | Trash |
| Root | `.VolumeIcon.icns` | VolumeIcon |
| Root | `.hidden` | HiddenList |
| Root | `.metadata_never_index`, `.metadata_never_index_unless_rootfs` | IndexingMarker |
| Root | `.com.apple.timemachine.donotpresent`, `.com.apple.timemachine.supported` | TimeMachineMarker |
| Root | `.apdisk` | AfpShare |
| Root | `.HFS+ Private Directory Data\r`, `HFS+ Private Data` after four NUL bytes (also as four `0x1A`, the index's stand-in), `.journal`, `.journal_info_block`, `.hotfiles.btree` | HfsPrivate, HfsJournal |
| Root, **only when `hidden`** | `Desktop DB`, `Desktop DF` | ClassicDesktopDb |
| Root, only when `hidden` | `Desktop Folder` | ClassicDesktopFolder |
| Root, only when `hidden` | `Trash`, `Network Trash Folder` | Trash |
| Root, only when `hidden` | `Temporary Items`, `TheVolumeSettingsFolder`, `TheFindByContentFolder`, `Cleanup At Startup`, `Shutdown Check`, `AppleShare PDS`, `Move&Rename`, `VM Storage`, `OpenFolderListDF\r` | ClassicHousekeeping |
| Anywhere | `Thumbs.db`, `desktop.ini` | Windows… |
| Root | `$RECYCLE.BIN`, `System Volume Information` | Windows… |
| Root | `lost+found` (folder), `.Trash-` followed by digits | Linux… |

The "only when hidden" guard keeps a user's own folder called "Trash" on a modern disk in view: the classic names count only when the Finder itself hid them.

`.hidden` at the root (Mac OS X 10.0 to 10.3) lists names to hide. When the root is listed, the app reads it (at most `MAX_HIDDEN_LIST = 64 KiB`, 1024 lines of at most 255 bytes) and treats those root names as hidden.

**Where the rule applies** (`crates/app/src/state/visibility.rs`, new, one function used by all): folder rows, icon view, search results (a "hidden items" filter already follows `show_hidden`), the space map's labels, Quick Look's previous and next. **Where it does not:** sizes. Hidden and housekeeping bytes are always counted, or folder sizes would lie.

**The status bar** says "17 items · 4 hidden". "4 hidden" is a button: it shows them in this folder until you leave it. Its tooltip names the two settings.

**Copying a folder out** (`extract`, which already has `options.hidden`): housekeeping items inside are left behind unless `copy_system_items` is on (default off). Items the user selected by name are always copied (decision 34).

### 5.2 What each item does on a Mac, and here

| Item | On a Mac | In Disk Atlas, this plan |
|---|---|---|
| `._name` (AppleDouble) | Carries `name`'s resource fork and Finder information on a disk that has no forks (FAT, exFAT, NTFS, a server) | **Joined to its file.** For a file on a volume without forks, details, Quick Look and Properties look up `._name` beside it and show its resource fork (the fork switch appears), type and creator, flags, label and custom icon as the file's own. Copy out with "Keep resource forks" copies the sidecar along. Setting `merge_sidecars` (default on). A `._name` with no `name` beside it is a housekeeping item |
| `__MACOSX/` in archives | The same, inside a zip | The archive listing of Q1 hides it and marks the files it describes "has resource fork" |
| `.DS_Store` | The Finder's notes on one folder: view, icon places, window, background, comments, put-back places for the Trash | **Read** by a bounded reader (§5.3). Used for: Finder comments in Properties; "Was in" for items in a Trash; the folder's own view and sort when `finder_views` is on (default off); and "As arranged", the last task of R17 (§5.5) |
| `.Trashes`, `.Trash`, classic `Trash`, `$RECYCLE.BIN`, `.Trash-1000` | What was thrown away and not yet erased | **A "Trash" row under the volume** in the sidebar when it holds anything. It opens the real folder (one user: straight in; several: one folder per user, labelled "User 501"). A "Was in" column comes from `.DS_Store` (`ptbL`), from Windows `$I` files, or from Linux `.trashinfo`. Everything in it can be previewed and copied out like any file. Disk Atlas never empties it |
| classic `Desktop Folder` | The files that sat on that disk's desktop | **A "Desktop" row under the volume.** It is hidden on disk but it is the user's own files, so it must not disappear with the housekeeping |
| `.VolumeIcon.icns` | The volume's own icon | The volume's icon in the sidebar, disk view and Properties (ICNS reader from Q1) |
| `Icon\r` | A folder's custom icon, in its resource fork | The folder's icon through the thumbnail pipeline (R6), when the folder has the custom-icon flag |
| `.localized` | Tells the Finder to show a translated name | Hidden. A folder named `Name.localized` is shown as "Name"; Properties gives the stored name |
| `.hidden` | Old list of names to hide | Honoured at the root (§5.1) |
| Packages (`.app`, `.bundle`, the bundle bit) | Shown as one file | Shown as one item (§5.4) |
| Aliases and symbolic links | Point somewhere else | "Show original" (§5.4) |
| Finder labels and tags | Colours | A dot before the name (§5.4); names in Properties |
| `com.apple.quarantine`, `kMDItemWhereFroms`, `kMDItemFinderComment`, `_kMDItemUserTags` | Download origin, comments, tags | Read into Properties in words (§7) |
| `.metadata_never_index`, Time Machine and AFP markers | Switches for Apple's services | Hidden; named as facts in the volume's Properties ("Spotlight was told not to index this volume") |
| `.Spotlight-V100` | Spotlight's index | Hidden, counted under System. Disk Atlas has its own search and does not read Apple's store (no public format) |
| `.fseventsd` | A log of what changed on the volume | Hidden, counted under System. A "recent changes" view from it is possible later, not planned |
| `.DocumentRevisions-V100` | Saved versions of documents | Hidden, counted under System. Browsing versions is not planned |
| HFS+ private folders, journal, hot-file tree | The filesystem's own records | Hidden, counted under System |
| `Desktop DB`, `Desktop DF` | Classic Finder's icon and comment database | Hidden. Classic comments are not read in this plan |
| `Thumbs.db`, `desktop.ini`, `System Volume Information`, `lost+found` | Windows and Linux housekeeping | Hidden under the same switch. `desktop.ini`'s folder names and icons are not read in this plan |

### 5.3 Readers (`crates/lib/src/mac/`, new module: Mac conventions that are not a filesystem)

Each takes bytes from a disk: bounded, checked arithmetic, errors and never panics, a fuzz target and seeds each (root `CLAUDE.md`, "Commands").

- **`appledouble.rs`** (reader; the writer stays in `extract/appledouble.rs`). Layout checked against RFC 1740 on 2026-11-13; every integer is big-endian. Magic `0x00051607` (AppleDouble) or `0x00051600` (AppleSingle: the same header with the data fork inside it as entry 1), version `0x00020000`, 16 filler bytes, entry count u16 (≤ `MAX_AD_ENTRIES = 64`), then entries of id u32, offset u32, length u32. Entry 2 is the resource fork, 9 the Finder information (32 bytes, then on macOS an extended-attribute block: ignore past byte 32 in this plan). Every offset and length is checked against the sidecar's size.

  ```rust
  pub struct Sidecar { pub finder_info: Option<[u8; 32]>, pub resource: Option<(u64, u64)> /* offset, len */ }
  /// Looks up `._name` in `dir`, reads at most `MAX_AD_HEADER = 4096` bytes.
  pub fn read_sidecar(fs: &dyn FileSystem, dir: NodeId, name: &str) -> Result<Option<(NodeId, Sidecar)>>;
  /// The resource fork as a window on the sidecar's data stream.
  pub fn open_resource(fs: &dyn FileSystem, sidecar: NodeId, at: (u64, u64)) -> Result<Arc<dyn BlockSource>>;
  ```
  Only called when `VolumeInfo::resource_forks` is false, for the one file being shown, off the UI thread. The list does not look up a sidecar per row.

- **`bplist.rs`** (binary property list; pulled here from Q1, which reuses it). Magic `bplist00` (8 bytes). The last 32 bytes are the trailer: 6 bytes unused, offset size u8, reference size u8 (each 1, 2, 4 or 8), object count u64, root object number u64, offset table position u64, all big-endian. The offset table holds one position per object, each "offset size" bytes wide. An object starts with a marker byte, high four bits the type, low four bits `n`:

  | Marker | Object |
  |---|---|
  | `0x00`, `0x08`, `0x09` | null, false, true |
  | `0x1n` | integer of 2ⁿ bytes, big-endian (8 bytes is signed; accept n ≤ 3, refuse 16-byte integers) |
  | `0x22`, `0x23` | real: 4 or 8 bytes, IEEE, big-endian |
  | `0x33` | date: 8-byte float, seconds since 2001-01-01 00:00 UTC |
  | `0x4n` | data: `n` bytes follow |
  | `0x5n` | ASCII string of `n` bytes |
  | `0x6n` | UTF-16BE string of `n` units |
  | `0x8n` | UID of `n + 1` bytes (kept as an integer) |
  | `0xAn` | array: `n` object numbers follow, each "reference size" bytes |
  | `0xDn` | dictionary: `n` key numbers, then `n` value numbers |

  For data, strings, arrays and dictionaries, `n = 15` means the real count follows as an integer object (a `0x1n` marker and its bytes). A set (`0xCn`) is read as an array. Every position, count and object number is checked against the file before use; an unknown marker is an error. (Written from Apple's `CFBinaryPList.c`, as remembered: test against three real files, one of them from a `.DS_Store` `bwsp` blob.) Bounds: input ≤ `MAX_BPLIST = 1 MiB`, objects ≤ 65,536, depth ≤ 32, strings ≤ 64 KiB, a visited set so a container cannot hold itself. Output: a small owned tree (`Plist::{Bool, Int, Real, Date, Data, String, Array, Dict}`).

- **`ds_store.rs`** (O). The file is a buddy allocator holding a B-tree. Checked on 2026-11-13 against Wim Lewis's format notes (`DSStoreFormat.pod` in the `Mac::Finder::DSStore` distribution); test on two real files all the same. Every integer is big-endian. 4 bytes `00 00 00 01`; magic `Bud1`; root block offset u32, size u32, offset again u32 (must match); every offset is from byte 4. After the header's three numbers come 16 bytes of unknown use. The root block holds a count u32, 4 unknown bytes, then the block address table (each u32: offset = address with the low 5 bits cleared, size = 1 << low 5 bits), which is padded so its length is a multiple of 256 entries; then a directory: count u32, and per tree a name length u8, the name, a block number u32 (`DSDB` is the one that matters); then 32 free lists, each a count u32 and that many u32 offsets. The `DSDB` block gives the root node, levels, record count, node count and page size. A node is: rightmost-child block u32 (0 = leaf), count u32, then records, each preceded by a child block u32 when the node is not a leaf. A record is: name length u32 (UTF-16 units), the name in UTF-16BE, a four-character code, a four-character type, a value: `bool` 1 byte, `long` and `shor` 4 bytes, `type` 4, `comp` and `dutc` 8, `blob` u32 length + bytes, `ustr` u32 length + UTF-16BE. `shor` keeps its value in the last two of its four bytes; `dutc` counts 1/65536 of a second since 1904-01-01. Records are sorted by name without regard to case, then by code, and a code has one type everywhere, so a record whose type does not fit its code is an error for that record only. The folder's own records are under the name `.`. Of the codes used here, the notes cover `Iloc` (a 16-byte blob: x and y of the icon's **centre** as two u32, then six `0xFF` and two zero bytes), `cmmt` (`ustr`), `vstl` (`type`: `icnv`, `Nlsv`, `clmv`, `Flwv`), `icvp`, `bwsp`, `lsvp` and `lsvP` (blobs holding a binary plist; `bwsp`'s `WindowBounds` is a string like `{{x, y}, {w, h}}`) and `BKGD` (12 bytes: `DefB`, or `ClrB` and three u16 of red, green, blue, or `PctB`). They do not cover the put-back codes `ptbL` and `ptbN` (both `ustr`, seen in a Trash folder's file): **verify those first** on the `.DS_Store` of a real `.Trashes/501`.
  Bounds: file ≤ `MAX_DS_STORE = 16 MiB`, blocks ≤ 65,536, records ≤ `MAX_DS_RECORDS = 100_000`, depth ≤ 16, a blob ≤ 1 MiB, a name ≤ 255 units, a visited set of block numbers.

  ```rust
  pub struct DsStore { /* records sorted by (name, code) */ }
  impl DsStore {
      pub fn parse(bytes: &[u8]) -> Result<Self>;
      pub fn comment(&self, name: &str) -> Option<&str>;              // cmmt
      pub fn put_back(&self, name: &str) -> Option<(&str, &str)>;     // ptbL folder, ptbN name
      pub fn icon_place(&self, name: &str) -> Option<(i32, i32)>;     // Iloc: x, y as u32 BE in the first 8 bytes
      pub fn view_style(&self) -> Option<ViewStyle>;                  // vstl of ".": icnv, Nlsv, clmv, Flwv, glyv
      pub fn icon_view(&self) -> Option<IconView>;                    // icvp bplist: iconSize, backgroundType, colour, picture alias
      pub fn window(&self) -> Option<(i32, i32, u32, u32)>;           // bwsp bplist: WindowBounds
      pub fn list_sort(&self) -> Option<(SortColumn, bool)>;          // lsvp / lsvP bplist: sortColumn
  }
  ```
  The sizes it records for folders (`lg1S`, `ph1S`) are ignored: they are old.

- **`resource_map.rs`** (O; pulled here from Q1, which reuses it for icons and fonts). Checked on 2026-11-13 against the reader in the `python-rsrcfork` library, which follows *Inside Macintosh: More Macintosh Toolbox*; every integer is big-endian.

  | Where | Field |
  |---|---|
  | Fork + 0 | data offset u32 (almost always 256), map offset u32, data length u32, map length u32; then 112 system bytes and 128 application bytes, ignored |
  | Map + 0 | 16 bytes (a copy of the header, often zero), 4 bytes, 2 bytes: all ignored |
  | Map + 22 | file attributes u16 |
  | Map + 24 | type list offset u16, **from the start of the map** |
  | Map + 26 | name list offset u16, from the start of the map |
  | Type list + 0 | number of types − 1 as u16 (`0xFFFF` means none) |
  | Type list + 2, 8 bytes each | type (4 bytes, any bytes), number of resources − 1 as u16, reference list offset u16 **from the start of the type list** |
  | Reference, 12 bytes each | id i16 (the only signed field), name offset u16 from the start of the name list (`0xFFFF` = no name), attributes u8, data offset u24 **from the start of the resource data**, 4 bytes ignored |
  | Data offset + that | length u32, then the bytes |
  | Name | length u8, then that many bytes in Mac OS Roman |

  Bounds: types ≤ 2,048, resources per type ≤ 16,384, total ≤ 65,536, one resource read ≤ a `max` given by the caller. The header's four numbers are checked against the fork's length and against each other (data and map must not overlap) before anything else is read.

  ```rust
  pub struct ResourceMap { /* (type, id) -> (data offset, name offset), sorted */ }
  impl ResourceMap {
      /// Reads the header and the map (at most `MAX_MAP_BYTES = 1 MiB`), no resource data.
      pub fn read(fork: &dyn BlockSource) -> Result<Self>;
      pub fn types(&self) -> impl Iterator<Item = ([u8; 4], usize)> + '_;
      pub fn ids(&self, kind: [u8; 4]) -> impl Iterator<Item = i16> + '_;
      /// `Err` when the resource is longer than `max`.
      pub fn load(&self, fork: &dyn BlockSource, kind: [u8; 4], id: i16, max: usize) -> Result<Option<Vec<u8>>>;
  }
  ```

- **`alias.rs`** (O). The classic alias record from resource `alis` id 0 of a file with the alias flag: version 2 or 3; reads the target's file number, its parent folder id, the volume name and creation date, and the tagged strings after the fixed part (tag 2: absolute path, 18: POSIX path, 19: POSIX path of the volume mount). The version 2 fixed part is 150 bytes, big-endian (from the published layout of the alias record, read on 2026-11-13; test with one alias made on a real Mac):

  | Offset | Size | Field |
  |---|---|---|
  | 0 | 4 | creator code of the application that made it, or 0 |
  | 4 | 2 | record size, the tagged data included |
  | 6 | 2 | version (2) |
  | 8 | 2 | kind: 0 file, 1 folder |
  | 10 | 1 + 27 | volume name: length byte, then Mac OS Roman, zero-padded |
  | 38 | 4 | volume creation date, seconds since 1904 |
  | 42 | 2 | volume signature (`BD` HFS, `H+` HFS+) |
  | 44 | 2 | volume type: 0 fixed disk, 1 network, 2 to 4 floppies, 5 other ejectable |
  | 46 | 4 | **id of the folder that holds the target** |
  | 50 | 1 + 63 | target name: length byte, then Mac OS Roman |
  | 114 | 4 | **the target's file number** |
  | 118 | 4 | target creation date |
  | 122 | 4 + 4 | type, creator |
  | 130 | 2 + 2 | levels from the alias up to the common folder, and down to the target (−1: another volume) |
  | 134 | 4 | volume attributes |
  | 138 | 2 | filesystem id |
  | 140 | 10 | reserved |

  Then tagged data until the record size: tag i16, length u16, the bytes, and one pad byte when the length is odd. Tag −1 ends the list. Tags: 0 the holding folder's name, 1 the ids of the folders above it (u32 each), 2 the absolute path with colons, 3 to 5 AppleShare zone, server and user, 6 driver name, 9 and 10 network details. Later systems add 14 (target name, UTF-16BE after a u16 count), 15 (volume name, the same), 18 (POSIX path) and 19 (POSIX path of the mount point): these four are from memory, so **verify them first** on an alias made by Mac OS X. A record whose version is 3 has a different fixed part that is not written here: report it as "This alias uses a newer format" unless the task's verify step finds its layout in a published reader (the `mac_alias` library documents it) and tests it on a real file.

  ```rust
  pub struct AliasTarget {
      pub is_folder: bool,
      pub volume_name: String,
      pub volume_created: Option<Timestamp>,
      pub parent: NodeId,
      pub node: NodeId,
      pub name: String,
      /// Tag 18, else tag 2 with colons turned into slashes.
      pub path: Option<String>,
      pub other_volume: bool,
  }
  /// Names are decoded as Mac OS Roman until M6's script encodings exist;
  /// tags 14 and 15, when present, win over the Pascal strings.
  pub fn parse_alias(record: &[u8]) -> Result<AliasTarget>;
  ``` Bounds: record ≤ 64 KiB, tags ≤ 64, a tag ≤ 4 KiB. Modern bookmark files (magic `book`) are recognised and reported "This alias uses a newer format"; not read in this plan.

- **`trash.rs`** (S). Windows `$I…` beside `$R…` (checked on 2026-11-13 against the libyal format notes; little-endian): version u64 at 0 (1 or 2), original size u64 at 8, deletion time u64 at 16 (100 ns steps since 1601, UTC). Version 1: the original path at 24, UTF-16LE, ending at a zero unit, at most 260 units. Version 2: a u32 count of units at 24, the path at 28; the notes do not say whether the count includes the closing zero, so stop at the first zero unit or at the count, whichever comes first, and at 32,767 units at most. `$IABC123.txt` describes `$RABC123.txt` (the same characters after the second one; from memory, confirm on a real bin). A `$R` with no `$I` is listed under its own name with "Was in" empty. Linux `info/<name>.trashinfo`: text, `Path=` (percent-encoded) and `DeletionDate=`. Both ≤ 4 KiB read. **Verify first** against two real files each.

- **`xattr.rs`** (S). `com.apple.quarantine`: text `flags;hex seconds since 1970;agent;uuid`. `com.apple.metadata:kMDItemWhereFroms` and `…:_kMDItemUserTags`: bplist arrays of strings (a tag is "Name\nN", N the colour 0 to 7). `…:kMDItemFinderComment`: a bplist string. Values read with `read_xattr(.., max = 64 KiB)`.

### 5.4 Packages, aliases, labels

- **Packages.** `mac::bundle::is_package(name, kind, flags)`: a folder with `EntryFlags::PACKAGE`, or whose extension is one of `app bundle plugin kext prefPane qlgenerator mdimporter saver wdgt component action workflow pkg mpkg rtfd photoslibrary xcodeproj playground dSYM sparsebundle scptd download`. A package is one row with its kind's glyph ("Application"), its total size, and its own icon when `Contents/Info.plist` names one (`CFBundleIconFile` in `Contents/Resources`, read through the XML or binary plist reader, at most 1 MiB). Enter or Space opens its Quick Look (name, version, identifier, size, contents by kind). "Show package contents" in the menu goes inside; the breadcrumb then shows it like a folder. Setting `packages_as_folders` (default off) turns the whole behaviour off. A sparse bundle on the host also gets "Open in Disk Atlas".
- **Show original.** For a symbolic link: resolve `Metadata::link_target` from the link's folder (or the root when absolute) with `lookup`, at most `MAX_LINK_STEPS = 64` segments, never leaving the volume. For a classic alias: the target's file number, found in the index (`Index::find_node`, then `crumbs`), checked against the alias's volume name; else the alias's path by `lookup`. Then navigate to the target's folder and select it, as "Show in folder" does for a search hit. When the target is on another volume: "The original is on a disk called Archive 2009."
- **Labels.** A 8 px dot before the name for `EntryFlags::label()` 1 to 7. Colours are fixed data colours, not theme tokens, each with a ring of `line` so it reads on both themes: 1 Gray, 2 Green, 3 Purple, 4 Blue, 5 Yellow, 6 Red, 7 Orange (the Mac OS X order; classic names Essential to Project 2 are given in Properties when the volume is classic HFS). The tooltip is the name. Setting `label_dots` (default on). None is the overprint magenta: label 3 is drawn `#8E5BD9`, clearly violet.

### 5.5 "As arranged" (last task of R17, droppable: decision 35)

A disk image made to be opened in the Finder (an installer window with an arrow to Applications) loses its meaning in a plain list. When a folder's `.DS_Store` says icon view and holds a place for at least half of its visible items, the icon view (R6) offers a chip "As arranged": items at their stored places on a canvas with the stored icon size (16 to 512, clamped), the background colour when one is stored, and the background picture when its alias resolves inside the same volume and decodes within 4096 × 4096. Read-only: nothing can be dragged. At most `MAX_ARRANGED = 2000` items; places are clamped to 0..=16,384; items without a place flow below. Setting `finder_layout`: `images` (default: on by itself for the root of a disk image), `always`, `never`.

## 6. Context menus

### 6.1 One menu component (`components/menu.rs`, new)

```rust
pub enum MenuItem {
    Action { label: SharedString, action: Box<dyn Action>, glyph: Option<Glyph>,
             checked: Option<bool>, enabled: Enabled },
    Submenu { label: SharedString, glyph: Option<Glyph>, items: Vec<MenuItem> },
    Separator,
}
pub enum Enabled { Yes, No { reason: SharedString } }

/// What was right-clicked. `menus::build` is a pure function of this.
pub enum MenuTarget {
    Rows { source: SourceFacts, rows: Vec<RowFacts>, in_search: bool },
    Folder { source: SourceFacts, folder: RowFacts },            // empty space
    ColumnHeader, SidebarDisk(..), SidebarVolume(..), SidebarImage(..),
    SidebarOtherPartition(..), SidebarPlace(..), Crumb(..),
    BarPartition(..), BarKind(..), MapRegion(..), QuickLook(..), TextField,
}
pub fn build(target: &MenuTarget, state: &MenuState) -> Vec<MenuItem>;   // state/menus.rs

/// The place. Filled by the caller from `Sources`; `build` never asks anything else.
pub struct SourceFacts {
    pub id: SourceId,
    /// `SourceKind::Host`: the only place file operations exist.
    pub host: bool,
    /// HFS, HFS+, HFSX: offers "Mac path with colons".
    pub hfs_family: bool,
    /// The size table is in place: offers the space map and "Show the largest…".
    pub has_sizes: bool,
    /// Sizes are off here (the setting, or a network drive): offers "Calculate size".
    pub sizes_off: bool,
    pub has_trash: bool,
}
pub struct RowFacts {
    pub node: NodeId,
    pub name: SharedString,
    pub kind: NodeKind,
    pub package: bool,
    /// An alias or a symbolic link: offers "Show original".
    pub points_elsewhere: bool,
    /// A disk image or sparse bundle on the host: "Open in Disk Atlas" comes first.
    pub disk_image: bool,
    pub size: Option<u64>,
}
pub struct MenuState {
    pub built: Built,
    pub can_paste: bool,
    pub last_copy_destination: Option<SharedString>,
    pub view: ViewMode,
    pub sort: (SortColumn, bool),
    pub folders_first: bool,
    pub show_hidden: bool,
    pub show_system: bool,
    pub columns: ColumnSet,
    /// "Explorer", "Finder" or "Files".
    pub file_manager: &'static str,
}
/// Which features this build has. One `const BUILT: Built` in `state/menus.rs`;
/// the milestone that builds a feature turns its field on, in the same change.
/// An item whose feature is off is left out, never greyed.
pub struct Built {
    pub properties: bool,      // R18
    pub host_operations: bool, // R8
    pub space_map: bool,       // R7
    pub layout_mode: bool,     // R15.2
    pub pins: bool,            // M8 §8.4
    pub open_copy: bool,       // R11.2, decision 37
    pub trash_rows: bool,      // R17a
    pub show_original: bool,   // R17a
}
```

- **A chosen item dispatches its action** (`window.dispatch_action`, `window.rs:1476`) to the element that was focused before the menu opened. A menu item and its key binding are therefore the same code path, and the shortcut shown at the right of an item is read from the keymap (`window.bindings_for_action`, `window.rs:4223`), never typed by hand.
- **Left out or greyed.** An item that can never apply to this kind of place is left out (nothing about renaming on a read-only disk). An item that applies but not right now is greyed and its reason is its tooltip ("Nothing to paste").
- **Opening.** Right mouse button down (`MouseButton::Right`, `interactive.rs:289`); the Menu key (`"menu"`, `platform/windows/events.rs:1376`) and Shift+F10, which open at the cursor row. On a row that is not selected, the click selects it first; on a selected row the selection stays; on empty space the selection clears. On macOS GPUI already turns Ctrl+click into a right click (`platform/mac/window.rs:1793`), so rows handle one button. The Menu key is `"menu"` on Windows and on Linux (there GPUI lower-cases the key's xkb name, `platform/linux/platform.rs:881`); a Mac keyboard has none, so on macOS the keyboard way in is Shift+F10 only.
- **Drawing.** `deferred(anchored().position(p).snap_to_window_with_margin(px(8.)))` (`elements/anchored.rs:47`, `:74`), elevation level 2, radius 10, padding 4, width 220 to 360, items 28 px high, glyph column 16 px, shortcut in `ink3` at the right, separators 1 px `line` with 4 px above and below. Fades in with a 4 px rise over `QUICK`; instant with reduced motion.
- **Submenus** open to the right after a 150 ms rest or at once on Right or Enter, flip left at the window's edge, close on Left. One level deep only.
- **Keys** (context `MENU`): Up, Down, Home, End; Right and Left for submenus; Enter or Space chooses; Escape closes one level; a letter moves to the next item that starts with it. Focus goes back to where it was.
- **Closing.** A click outside closes the menu and is not passed on (a transparent backdrop, as the filter menus do at `toolbar.rs:539`); a right click outside closes it and opens the menu for the new place. Also closed by the window losing focus, by scrolling and by navigation.
- The filter menus of search move onto this component with no change in behaviour (their tests stay green).

### 6.2 What each menu holds

Shortcuts are shown from the keymap; the ones written here are the bindings to add. "(R8)" marks items that arrive with host file operations.

**Files and folders on a disk Disk Atlas reads** (read-only):

| Item | Notes |
|---|---|
| Open | A folder. Enter |
| Quick Look | Space |
| Show package contents | Packages |
| Show original | Aliases and links |
| — | |
| **Copy to this computer…** | The app's primary action. Ctrl+Shift+C |
| Copy to ▸ | Desktop, Downloads, Documents, the last destination, "Choose…" |
| Open a copy in its app | One file up to `MAX_OPEN_COPY = 2 GiB`: copied to `<cache>/opened/<session>/`, then `cx.open_with_system` (`app.rs:1143`). The folder is emptied on quit and at the next start. Decision 37 |
| — | |
| Copy as ▸ | Name; Path; Mac path with colons (HFS family); Node id |
| Show on the space map | From R7 |
| Show where it is on the disk | Opens the disk view in Layout mode with the file's pieces |
| Pin to sidebar | Folders, from M8 §8.4 |
| — | |
| Properties | Alt+Enter |

**Files and folders on this computer:** Open (folder: Enter; file: its app, Shift+Enter) · Quick Look · Open in Disk Atlas (disk images and sparse bundles; first in the menu for them) · Open with… (Windows only for now: starts `rundll32.exe shell32.dll,OpenAs_RunDLL <path>`, no `unsafe`; verify first) · Show in Explorer / Finder / Files (`cx.reveal_path`, `app.rs:1138`) · — · Cut, Copy, Paste, Duplicate (R8) · Rename F2, Move to Recycle Bin Delete (R8) · — · Copy as ▸ Name, Path · Calculate size (when sizes are off for this place) · Show on the space map · — · Properties.

**Several items selected:** the same, without Open, Rename and Show original; labels count ("Copy 14 items to this computer…"). Properties opens one window for all of them.

**Empty space in a folder:** New folder Ctrl+Shift+N (R8) · Paste (R8) · — · View ▸ List, Icons, Space map · Sort by ▸ Name, Modified, Size, Kind, then Ascending, Descending, then Folders first ✓ · Show hidden items ✓ · Show system items ✓ · — · Refresh F5 · Copy this folder's path · Show in Explorer (host) · — · Properties (of the folder).

**Column header:** a check list of the columns (Modified, Created, Size, Resource fork, Kind, Label), then "Fit columns". The set is the `columns` setting.

**Sidebar.** Disk: Show this disk · Allow reading disks (when it needs permission) · Copy device path · Properties. Volume: Open · Search this volume · Show Trash (when it has one) · Rebuild the index · Properties. Disk image: the volume's items, then Close image · Show the image file in Explorer. Other partition: Properties (its reason is the first line there). Place on this computer: Open · Show in Explorer · Remove from sidebar (pins).

**Elsewhere.** Breadcrumb segment: Open · Copy path · Properties. Storage bar and space map: §4 and R7 (Open in the list · Zoom in · Copy path · Properties). Quick Look: Copy (text selection or the picture) · Copy to this computer… · Properties. A search hit: the file's menu plus "Show in its folder" Ctrl+Enter. Text fields: Cut · Copy · Paste · Select all.

**Not offered, on purpose** (decision 41): Eject (a command to a device: rule 1), the Windows shell's own menu with other programs' entries (COM, `unsafe`, slow), changing permissions or attributes.

### 6.3 Keys this plan adds

Checked against `keymap.rs` on 2026-11-13. "Ctrl" is `secondary` (Cmd on macOS) unless the row says otherwise. Each row is added by the milestone named, with the matching row in `ui-design.md`, "Keyboard".

| Key | Action | Context | With | Note |
|---|---|---|---|---|
| Menu key, Shift+F10 | `menu::Open` at the cursor row or the focused item | `FileList`, sidebar, breadcrumb, storage bar | R11 | Free today |
| Up, Down, Home, End, Left, Right, Enter, Space, Escape | The menu's own (§6.1) | `Menu` (new) | R11 | `FilterMenu` (`keymap.rs:36`) is retired: its six actions move to `menu::`, and the Kind menu keeps toggling on Enter and Space |
| A letter | Next item that starts with it | `Menu` | R11 | Handled in the menu's key-down handler, as the list handles typing a name; not a binding |
| F5 | `workspace::Refresh` | global | R11 | Free today |
| Shift+Enter | `file_list::OpenInApp` (host files) | `FileList` | R11 | Free today. Enter stays `Open` (`keymap.rs:235`) |
| Ctrl+Shift+C | `file_list::CopyOut` | `FileList` | R11, if M5's copy task has not bound it | In `ui-design.md` since M5; not in `keymap.rs` yet |
| Ctrl+, | `workspace::OpenSettings` | global | R10b | Free today |
| Ctrl+Shift+. | `workspace::ToggleHidden` | global | R16 | Free today |
| Alt+Enter | `workspace::Properties` | `FileList`, sidebar, breadcrumb | R18 | Free today |
| Ctrl+I | `workspace::Properties` | global | R18 | **Taken today** by `ToggleDetails` (`keymap.rs:247`), which moves to Ctrl+Shift+I (free). On macOS this makes Cmd+I "Get Info", as a Mac user expects |
| Escape, Ctrl+W | `aux::Close` | `AuxWindow` (new) | R5 | Quick Look's own Escape and Space stay as R1 §5.1 says |
| Ctrl+F | Focus "Find a setting" | `Settings` (new) | R10b | Shadows the global `Search` (`keymap.rs:252`) in that window only: the deepest context wins |
| Ctrl+3 | Space map view | global | R7 | R1 §3.5 |
| F2, Delete, Ctrl+C, Ctrl+X, Ctrl+V, Ctrl+Z, Ctrl+Shift+N | Host file operations | `FileList` | R8 | R1 §4. The search field keeps its own Ctrl+C, X and V (`keymap.rs:288`) |

## 7. Properties

One window per item or selection (R5's `aux_window`), 420 × 560, resizable down to 360 × 400, at most `MAX_PROPERTIES_WINDOWS = 8`; asking again for the same item brings its window forward. Opened by Alt+Enter, by Ctrl+I (decision 36: the details pane's toggle moves to Ctrl+Shift+I) and from every menu. Escape or Ctrl+W closes it. It is called "Properties" on every system.

How it differs from what exists: the details pane is a glance at the selection; Quick Look's Details tab explains a file's structures; Properties holds every fact, stays open, and two of them can sit side by side. All three build their rows from one place: the fact builders in `components/details.rs` (`facts`, `tab_rows`, `:342`, `:743`) move to `state/facts.rs` and gain what is below.

Layout: a 96 px header (64 px icon or thumbnail; name at 15/700, a rename field on the host from R8; under it kind and size in `ink2`), then sections with 12/700 sentence-case heads in `ink3`, each collapsible, its state remembered. Rows are a 120 px label in `ink3` and a value in `ink`. No boxes around sections: 24 px of space separates them (R1: tone and space, not outlines). Right-click a row: "Copy" (the value) and "Copy everything" (the window as text). A value still being worked out shows its running figure in `ink3`.

| Section | Rows | Shown for |
|---|---|---|
| General | Kind · Size "18.2 GB (19,540,221,184 bytes)" · On disk (files: the sum of `extents`) · Contains "2,140 files, 96 folders" · Where (a link: shows it in the main window) · Created · Modified · Opened | Everything |
| What is inside | The storage bar's segments for this folder (`classes_of`), its legend, and the five largest items (click: select in the main window) | Folders, packages, volumes, several items |
| Mac information | Type and creator "TEXT · ttxt" · Label · Tags · Comments · Where from · Downloaded "with Safari, 12 Jan 2020" · Flags in words (Invisible, Alias, Custom icon, Stationery, Name locked, Package) · Resource fork size, with "from ._name" when it came from a sidecar | When any of them exists |
| On the disk | "3 pieces" with a 12 px Layout strip showing them in `pin` · the first eight pieces as byte offset and length, then "Show all" (opens Quick Look's Details) · Node id · Compressed "Yes" | Files on a disk Disk Atlas reads |
| Permissions | Owner and group as numbers ("501: names belong to the Mac that wrote the disk") · Mode "rw-r--r-- (644)". Host on Windows: Read-only, Hidden, System as Yes or No | When recorded |
| Extended attributes | Name and size; a row opens the value in Quick Look's Bytes view | When any |
| Checksum | "Calculate SHA-256": runs in the background in 1 MiB steps with a progress bar and Cancel; the result can be copied. Uses `sha2`, already in `Cargo.lock` and part of decision 9; the section is left out until that is approved | Files |

**A volume:** General (Format · Capacity · Used · Free · the storage bar · Files and folders, from the index) · Volume (Name · Identity as hex · Journal, in the words `details.rs:306` already uses · Case-sensitive, when known · facts from the markers of §5.2) · On the disk (partition number, start, length, and a link to its disk).

**A disk:** Model · Connection · Size · Sector size · Partition map (GUID, Master Boot Record, Apple) · Device path · Read-only: always · then the partitions as a table (name, format, start, size, "readable" or the reason not).

**Several items:** "14 items" · total size · What is inside · the folder they share.

Everything loads on the background executor, each section on its own, and each result carries the window's generation so a closed or retargeted window drops it. On the host, a watcher batch that names the item's folder refreshes it.

### 7.1 State (`state/properties.rs`, `state/facts.rs`)

```rust
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum PropertiesTarget {
    Item { source: SourceId, parent: NodeId, node: NodeId, name: SharedString },
    Items { source: SourceId, parent: NodeId, nodes: Arc<[NodeId]> },
    Volume { source: SourceId },
    /// The id `Sources` gives a physical disk or an image.
    Disk { disk: SourceId },
}

/// One row of any facts list: the details pane, Quick Look's Details tab, Properties.
pub struct Fact {
    pub label: SharedString,
    pub value: SharedString,
    /// What "Copy" puts on the clipboard when it differs from `value`
    /// (the exact byte count, not "18.2 GB").
    pub copy: Option<SharedString>,
    /// "Where" and "Disk" rows: clicking goes there in the main window.
    pub goes_to: Option<Location>,
    /// The value is still being worked out: drawn in `ink3`.
    pub running: bool,
}
pub enum SectionState { Loading, Ready(Vec<Fact>), Absent, Failed(SharedString) }
pub enum SectionId { General, Inside, Mac, OnDisk, Permissions, Xattrs, Checksum, Volume }

pub struct Properties { /* target, generation: u64, sections: [(SectionId, SectionState)], tasks */ }
impl Properties {
    pub fn new(target: PropertiesTarget, cx: &mut Context<Self>) -> Self;  // starts every loader
    pub fn as_text(&self) -> String;                                       // "Copy everything"
}

/// All open Properties windows, so a second request focuses the first.
pub struct PropertiesWindows { /* HashMap<PropertiesTarget, AnyWindowHandle> */ }
impl Global for PropertiesWindows {}
impl PropertiesWindows {
    /// Focuses the window for `target` or opens one; refuses past
    /// `MAX_PROPERTIES_WINDOWS` with the notice "Close a Properties window first."
    pub fn open(target: PropertiesTarget, cx: &mut App);
}
```

- A section that is `Absent` is not drawn. One that is `Failed` shows its head and the reason in `ink2` ("The extended attributes could not be read (attributes node 12)."), and the other sections stay.
- The loaders are plain functions in `state/facts.rs` from `(&dyn FileSystem, NodeId, …)` to `Vec<Fact>`, with no GPUI types, so each is unit-tested against a fixture volume. The details pane and Quick Look call the same functions.
- "What is inside" on a volume Disk Atlas reads calls `classes_of` and takes the five largest of `children` on the background executor. On the host it reads `HostSizes::classes`, `children` and `own`, and requests the folder at `Listed` priority if it is `Unknown`.

### 7.2 The window all three share (`views/aux_window.rs`, built in R5)

Quick Look (R5), Properties and Settings are separate windows with the same frame. GPUI calls read on 2026-11-13: `cx.open_window(WindowOptions, build)` returns `anyhow::Result<WindowHandle<V>>` (`app.rs:943`); `WindowOptions` has `window_bounds`, `titlebar`, `focus`, `kind`, `is_resizable`, `is_minimizable` and `window_min_size` (`platform.rs:1089`).

```rust
pub struct AuxOptions {
    /// The settings key its bounds are saved under: "window.quick_look",
    /// "window.settings", "window.properties".
    pub bounds_key: &'static str,
    pub title: SharedString,
    pub size: Size<Pixels>,
    pub min_size: Size<Pixels>,
    /// Properties: each new window opens 24 px down and right of the last
    /// one, and only the size is remembered.
    pub cascade: bool,
}

/// Opens a window with our own title bar (drag area, title in 12 px `ink2`,
/// close button), `WindowKind::Normal`, `chrome` title bar over a `sheet` body.
/// Restores the bounds saved under `bounds_key`, moved back onto a display
/// that still exists; saves them 500 ms after they stop changing. Puts the
/// `AuxWindow` key context on the root, so Escape and Ctrl+W close it.
pub fn open<V: Render + Focusable>(
    options: AuxOptions,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> anyhow::Result<AnyWindowHandle>;
```

Closing the main window closes every auxiliary window (R1 §6 says this for Quick Look; it holds for all three). Until the settings store exists (R10a comes before R5 in §10, so it does), bounds live for the session only.

## 8. Settings

### 8.1 The store (`state/settings.rs`; R10a)

The file, its place and its format are `m8-formats-plugins.md` §8.1 and decision 13, unchanged: `key = value` lines, unknown keys kept, bad values ignored with a warning, read once before the first window, written in the background to a temporary file and renamed.

New here: **one table describes every setting**, and the file reader, the settings window, its search and the reset buttons are all driven by it.

```rust
pub struct SettingDef {
    pub key: &'static str,
    pub section: Section,
    pub label: &'static str,
    /// One line under the label. Says what changes, in plain words.
    pub help: &'static str,
    pub kind: SettingKind,
    /// Left out of the window (and ignored in the file) elsewhere.
    pub platforms: Platforms,
}
pub enum SettingKind {
    Bool { default: bool },
    Choice { options: &'static [(&'static str, &'static str)], default: &'static str }, // value, label
    Int { min: i64, max: i64, default: i64, unit: &'static str },
    Hidden,   // stored, not shown: window bounds, last destination, pins
}
pub static SETTINGS: &[SettingDef];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section { General, Appearance, Browsing, FolderSizes, Disks, QuickLook,
                   Copying, Keyboard, Extensions, Advanced, About }
#[derive(Clone, Copy)]
pub struct Platforms { pub windows: bool, pub macos: bool, pub linux: bool }
impl Platforms { pub const ALL: Self; pub const fn here(self) -> bool; }
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue { Bool(bool), Choice(&'static str), Int(i64), Text(String) /* hidden keys */ }

pub struct Settings { /* one typed field per key */ }
impl Global for Settings {}
impl Settings {
    pub fn get(cx: &App) -> &Settings;
    /// Sets one key, repaints through `observe_global`, saves in the background.
    pub fn set(cx: &mut App, key: &'static str, value: SettingValue);
    pub fn reset(cx: &mut App, key: &'static str);
}
```

A test walks `SETTINGS` and fails on a duplicate key, an empty help line, or a default that is not among the options. Views read `Settings::get(cx)` and observe the global (`app.rs:1522`); nothing copies a setting into its own state.

### 8.2 The window (`views/settings.rs`; R10b)

A window of its own (R5's `aux_window`), 760 × 560, minimum 640 × 420, opened by Ctrl+, and from the top bar's menu. This replaces M8's "floating panel like Quick Look".

- **Left, 200 px on `chrome`:** a "Find a setting" field, then the sections with their glyphs. Typing filters rows across every section by label, help and option names; the list at the left then shows only sections with matches, each with its count.
- **Right, on `sheet`:** the section's name at 20/700, then its rows. A row is 56 px: label 13/400 in `ink`, help 12/400 in `ink2` under it, the control at the right. Groups inside a section are separated by 24 px and a 12/700 `ink3` head, not by lines.
- **Controls:** a switch for yes or no; a segmented control for two to four choices; a menu (§6.1) for more; a stepper with its unit for numbers; a plain button for actions. Controls are 32 px high (R1 §1.4). The switch is `ink` when on, `field` when off: it is never magenta (the overprint is for selection only).
- **Changes apply at once.** There is no OK, Apply or Cancel. A row that differs from its default shows a small `ink3` dot before its label and a "Reset" text button on hover or focus. Each section ends with "Reset this section".
- **Keys:** Up and Down in the section list; Tab through the rows; Space toggles; Left and Right move a segmented control; Ctrl+F focuses the search field; Escape clears the search, then closes.
- A setting that needs something not there yet says so in its help line and is greyed ("Needs Windows 11 22H2 or later").

### 8.3 Sections and settings

Keys already in M8 §8.1 are marked ·. "→" names where the behaviour is designed.

**General**

| Key | Control | Default | Help line |
|---|---|---|---|
| `start_at` | Last place · This computer | last place | Where Disk Atlas opens |
| `size_units` | As this system does · 1 GB = 1000 MB · 1 GB = 1024 MB | system (Windows: 1024; macOS and Linux: 1000) | How sizes are counted |
| `confirm_recycle` | switch | on | Ask before moving things to the Recycle Bin (R8) |

**Appearance**

| Key | Control | Default | Help line |
|---|---|---|---|
| `theme` · | System · Day · Night | system | |
| `density` | Compact · Regular · Roomy | regular | How tall rows are → R1 §1.4 |
| `motion` | System · Reduced · Full | system | → R1 §2.3 |
| `window_material` | switch | on | Let the desktop show through the sidebar and top bar → R1 §1.6 |
| `storage_bar` | Full · Compact · Hidden | full | The bar above the list that shows what fills the disk → §4 |
| `label_dots` | switch | on | Show Finder labels as coloured dots |

**Browsing**

| Key | Control | Default | Help line |
|---|---|---|---|
| `view` · | List · Icons | list | |
| `folders_first` | switch | on | Keep folders above files when sorting |
| `show_hidden` · | switch | off | Show files and folders the disk marks as hidden |
| `show_system_items` | switch | off | Show housekeeping files such as .DS_Store, ._ files and Thumbs.db → §5.1 |
| `merge_sidecars` | switch | on | Treat a ._ file as part of the file it belongs to → §5.2 |
| `packages_as_folders` | switch | off | Show applications and other packages as folders → §5.4 |
| `finder_views` | switch | off | Open each folder in the view and order the Finder last used → §5.2 |
| `finder_layout` | Disk images · Always · Never | disk images | Lay icons out where the Finder had them → §5.5 |
| `columns` | (set from the column header's menu) | Modified, Size, Kind | |
| `details_pane` ·, `sidebar` · | (set from the window) | on | |

**Folder sizes**

| Key | Control | Default | Help line |
|---|---|---|---|
| `folder_sizes` | Off · Folders I open · Whole drives | folders I open | On this computer, which folders are measured → §3.2 |
| `folder_sizes_network` | switch | off | Also measure network and removable drives |
| `watch_changes` | switch | on | Keep folders and their sizes up to date while files change → §3.3 |
| `index_on_open` | switch | on | Read a disk's whole file list when it is opened. Needed for folder sizes, the storage bar and instant search on disks Disk Atlas reads → §3.1 |

**Disks**

| Key | Control | Default | Help line |
|---|---|---|---|
| `permission_notice` | switch | on | Say at startup when disks need permission → R1 §3.1 |
| `other_partitions_open` | switch | off | Show system and unreadable partitions without a click → R1 §3.2 |
| `save_search_indexes` · | switch | on | |
| `cache_megabytes` · | stepper, 64 to 4096 MB | 256 | |
| (action) | "Delete saved indexes (182 MB)" | | |

**Quick Look:** `ql_keep_on_top` (switch, off, Windows and macOS) · `ql_remember_bounds` (switch, on) · `ql_model_view` (Shaded · Wireframe, shaded).

**Copying:** `keep_forks_on_copy` · (switch, off; decision 5's checkbox, same key) · `copy_system_items` (switch, off: "Also copy housekeeping files found inside folders") · `conflicts` (Ask · Keep both · Skip, ask; host operations, R8).

**Keyboard:** every shortcut, grouped as in `ui-design.md`, read from the keymap, with the search field filtering it. Read-only in this plan; rebinding is not designed.

**Extensions:** M8 §7.4, unchanged, appears with M8.

**Advanced:** `log_level` (Errors · Warnings · Everything, warnings) · actions: Open the settings file · Open the cache folder · Remove the permission to read disks (R9) · Reset every setting… (asks once).

**About:** the version, "GPL-3.0-or-later" with the source link (M8 §9), the fonts' licences, and the sentence "Disk Atlas never writes to a disk it reads itself."

Hidden keys: `last_copy_destination` ·, `pinned` ·, `recent_image` ·, `extension.<name>` ·, `hfs_encoding.<id>` ·, `window.main`, `window.quick_look`, `window.settings` (bounds), `window.properties` (size only), `properties.collapsed` (section names).

**In the file.** A bool is `true` or `false`; a choice is its value word (`folder_sizes = open_folders`); a number is decimal digits; bounds are `x,y,width,height` in whole pixels. A value outside its range, or a word that is not an option, is ignored with one warning line and the default is used; the line is kept as it was until the user changes that setting, so a newer version's value survives an older build.

## 9. Verify first

Facts this design rests on that must be confirmed before the code that uses them. Those marked "read" Fable confirmed in the source on 2026-11-13.

| Fact | State |
|---|---|
| `Index` holds parent and size per entry, pre-order, `parent[i] < i` | Read: `search/index.rs:20`, `:233`, `:257` |
| Saved index refuses another version | Read: `search/persist.rs:100`, `:241` |
| GPUI 0.2.2: `MouseButton::Right`; the `"menu"` key on Windows; `anchored().position`, `snap_to_window_with_margin`; `cx.reveal_path`, `cx.open_with_system`; `window.dispatch_action`, `bindings_for_action`; `observe_global` | Read: `interactive.rs:289`, `platform/windows/events.rs:1376`, `elements/anchored.rs:47` and `:74`, `app.rs:1138` and `:1143`, `window.rs:1476` and `:4223`, `app.rs:1522` |
| Ctrl+click as a right click on macOS | Read: GPUI does it, `platform/mac/window.rs:1793` and `:1819` |
| The Menu key's name | Read: `"menu"` on Windows (`events.rs:1376`) and Linux (xkb name lower-cased, `platform/linux/platform.rs:881`); none on macOS. Test on Linux with a real key: `client.rs:1014` (X11) and `:1328` (Wayland) have a second path that does not lower-case |
| How the HFS+ scan reports hard links | Read: once per link, with the target's node; the children of a linked folder hang from its first report, the `dir_<n>` in the private store (`fs/hfsplus/scan.rs:351`, `search/build.rs:23`, `:685`, `:709`). §3.1 is written for exactly this. Test with a linked-folder fixture |
| Where the index build starts today | Read: only `Search::activate` queues one (`state/search.rs:192`), through `start_next` (`:361`) and `start_build` (`:380`), one build at a time. R13.2 adds the second caller: opening a volume |
| `notify` 8.2.2: CC0-1.0, rust-version 1.88 (the toolchain here is 1.98); 7 crates not in today's `Cargo.lock` across all systems (`notify`, `notify-types`, `inotify`, `inotify-sys`, `kqueue`, `kqueue-sys`, `objc2-core-services`), 2 of them built on Windows | Measured by resolving it alone. From crates.io on 2026-11-13: released 2026-11-08, 58 M downloads in 90 days, 197 M in all; 9.0.0 is at its eighth release candidate, so pin `8.2` and expect one upgrade |
| `FileType::is_symlink` is true for Windows junctions | From the standard library's documentation; test with a real junction |
| `canonicalize` of a mapped network drive's root begins `\\?\UNC\` | From memory of how Windows resolves final paths; test with a mapped drive before relying on it (§3.2) |
| The AppleDouble, `.DS_Store`, resource map, alias version 2 and `$I` layouts | Checked against a published source each on 2026-11-13 (RFC 1740; Lewis's `.DS_Store` notes; the `python-rsrcfork` reader; the published alias record table; the libyal Recycle Bin notes). §5.3 carries the tables. Each still gets a real file in its tests |
| Binary property list markers; alias tags 14, 15, 18, 19 and alias version 3; `.DS_Store` codes `ptbL` and `ptbN`; how `$I` and `$R` names pair; the quarantine text | From memory: §5.3 marks each and says what to test it on |
| How the app knows a network or removable drive | Read: it does not (`state/sources.rs:157` is for physical disks). §3.2 says how to tell without `unsafe` |

## 10. Milestones and order

The gate of R1 §9 applies to every task with something to see: a screenshot of the running app in both themes, looked at by the lead. Parser tasks: a fuzz target that runs 60 s clean.

| # | Milestone | Owner | Depends on |
|---|---|---|---|
| R10a | Settings store and the `SETTINGS` table (§8.1, §8.3) | S | R2 |
| R10b | Settings window (§8.2) | S | R5 (`aux_window`), R11 |
| R11 | Menu component, filter menus moved onto it, context menus (§6) | S; lead reviews keys and focus | R2 |
| R12 | The housekeeping table (§5.1's function and names), one classifier, `ScanEntry::finder_type`, new flags, index version 2 (§2) | S (the table), O (HFS+ scan and flags), H (the mechanical field) | none; before R13 |
| R13 | `IndexSizes`, the index built at open, the Size column, sort by size (§3.1, §3.4) | O engine, S app | R12 |
| R14 | `HostSizes`, the watcher, live folder rows (§3.2, §3.3) | O engine and watcher, S app | R13; R3 (which drive is removable); decision 29 |
| R15 | Storage bar and Layout mode (§4) | S | R13; replaces the strip part of R3 |
| R16 | Hidden and system items in the app: the two switches, the rule, the status bar, copy-out (§5.1) | S | R10a, R12 |
| R17a | Mac conventions, tasks 1 to 6: sidecars, bplist, xattrs, packages, labels, Trash and Desktop rows, resource map, aliases, `.DS_Store` (§5.2 to §5.4) | S and O as in the briefs | R16 |
| R17b | Mac conventions, tasks 7 and 8: custom icons and package icons, "As arranged" (§5.4, §5.5) | S | R17a, R6, Q1's ICNS reader |
| R18 | Properties (§7) | S; lead reviews | R5, R11, R13; shows R17's facts as they land |

**One order for everything** (R1's milestones in plain type, new ones bold):

R1 → R2 → **R10a** → **R12** → R3 (sidebar and permission only; its strip is R15) → **R11** → **R13** → **R15** → **R16** → R4 → R5 → **R10b** → **R18** → **R14** → R6 → R7 → R8 → **R17a** → R9 → Q1 → **R17b** → G0 → G1 → Q2 → Q3.

Why this order: R10a comes early because R4, R5 and R16 store settings. R12 and R13 come before the bar and the space map because both read them. R11 comes before R8 so file operations land in menus that exist. R14 waits for the `notify` decision without blocking anything else. R17 is split because custom icons need the ICNS reader that Q1 builds; until R17b a package shows its kind's glyph. M6 still waits for R3.

**What Julian sees, and when.** Folder sizes on disks Disk Atlas reads and the new storage bar arrive at R15, the sixth step after approval; right-click menus at R11, the fifth. Properties and the settings window need the separate window that R5 builds. Folder sizes on this computer come at R14 and are the only part that waits for a new crate.

### 10.1 Briefs

The lead copies a brief, pastes in the interface from the section named, adds current line numbers, and sends it to the owner. Bounds are carried over word for word.

**R10a.1 (S) Settings store.** Owns `crates/app/src/state/settings.rs` (new), `main.rs` (load before the window). Build §8.1 and every key of §8.3 with its default; the file format of M8 §8.1. Tests: round trip; unknown keys survive; a bad value falls back and is logged; the table test of §8.1; a write is temp-then-rename. No window yet. Replace `DISK_ATLAS_REDUCED_MOTION`-only reading with the `motion` setting, keeping the variable as an override.

**R11.1 (S) Menu component.** Owns `components/menu.rs` (new), `keymap.rs` (the `MENU` context), `components/toolbar.rs` (move the filter menus onto it). §6.1 in full. Verify first: the GPUI lines in §9, and the two "not read" key facts. Tests (`TestAppContext`): opens at the pointer and stays inside the window; every key of §6.1; choosing dispatches the action to the previously focused element; focus returns; the existing filter-menu tests pass unchanged. Screenshot: a menu with a submenu, a greyed item and a checked item, both themes.

**R11.2 (S) What the menus hold.** Owns `state/menus.rs` (new), and the right-click handlers in `views/explorer.rs`, `components/file_row.rs`, `components/sidebar.rs`, `components/breadcrumb.rs`. `build` is pure. Tests: a table test, one case per target and source kind (read-only volume, host, search, several rows, empty space, each sidebar row), asserting the exact list of labels; an item for a feature that is not built yet is absent, not greyed. Add the actions and bindings: Properties (Alt+Enter, opens nothing until R18: leave the item out until then), Refresh (F5), Copy as, Show in Explorer, Open a copy in its app (decision 37; behind its approval).

**R12.0 (S) The housekeeping table.** Owns `crates/lib/src/mac/mod.rs`, `mac/conventions.rs` (new), `lib.rs` (the module). The enum, `system_item`, `label` and `explains` of §5.1, pure, no I/O. Tests: every row of the table; the root guard and the hidden guard both ways ("Trash" at the root and not hidden is a normal folder; `Desktop DB` in a sub-folder is a normal file); `._` alone is not a sidecar; the Windows rows match in any case and the Mac rows do not; the private-store names in both spellings (four NUL bytes and four `0x1A`). First, because R12.1 calls it.
**R12.1 (O) Classifier and flags in the engine.** Owns `search/kind.rs`, `search/index.rs` (flag bits), `search/build.rs`, `search/persist.rs` (`VERSION = 2`), `fs/mod.rs` (`ScanEntry::finder_type`, `EntryFlags` additions), `fs/hfsplus/scan.rs`, `fs/hfsplus/catalog.rs` (`entry_flags`, `:265`). §2 in full. Tests: every extension and type code in the tables; a classic file "ReadMe" with type `TEXT` is a Document; flags round-trip through `IndexFlags`; a version 1 saved index is refused; label, package and alias bits from an HFS+ fixture; bit 6 is set on `.Spotlight-V100` and on a file three levels inside it, and not on its sibling. Run the fuzz seeds test.
**R12.2 (H) The mechanical field.** Every other place that builds a `ScanEntry` gets `finder_type: None`; `cargo check --workspace`; report the list of files touched.
**R12.3 (S) The app uses the classifier.** Owns `format.rs`, `components/details.rs` (kind text keeps its labels; glyph, tint and the Kind filter use `classify`). No visible change except classic files getting their kind's glyph.

**R13.1 (O) `IndexSizes`.** Owns `crates/lib/src/sizes/mod.rs`, `sizes/index.rs` (new), `lib.rs` (the module). §3.1 in full, with `DENSE_NODE_LIMIT` and `MAX_LINK_NESTING`. Tests: a hand-built index with known totals; a file with three names counts three times in its folders and once in `volume`, classed by its visible name and not by `iNode<n>`; an orphan in the private store counts as `system`; the private stores are not in the root's total; a directory link shows its target's total and adds it to its own ancestors; a link inside a linked folder; a folder that links into itself ends at 0 with the lower-bound flag and no hang; the same on an index built from the HFS+ linked-folder fixture; a package's bytes go to its class; bytes under a housekeeping folder go to `system`; `children` and `folder_entry`; hostile arrays (a size of `u64::MAX − 1`, a parent out of order) saturate and never panic; `#[ignore]` timings on the 1,000,000-entry synthetic index of `search/bench.rs`, with and without 100,000 directory links, printed, against §3.6.
**R13.2 (S) Sizes in the app.** Owns `state/sizes.rs` (new), `state/search.rs` (build at open, `index_on_open`), `state/listing.rs` and `state/order.rs` (handles, sort by size, the re-sort rule), `components/file_row.rs` (the cell), `components/statusbar.rs`. §3.1 "When it runs" and "Changes", §3.4. Tests: a folder row shows its total after the build; sorting by size orders folders by total; the re-sort rule (no re-sort within 500 ms of a key); identity change rebuilds. Screenshot: a folder list with sizes on an HFS+ test image.

**R14.1 (O) `HostSizes`.** Owns `sizes/host.rs` (new). §3.2 in full with its constants. Tests on a temporary tree: totals match a plain recursive sum; a symbolic link and (Windows) a junction are not followed; an unreadable folder gives `AtLeast` up the chain; `request` priorities reorder work; `refocus` keeps what was measured; `invalidate` after adding, growing and deleting a file changes every ancestor by the right amount; `shutdown` joins; no thread outlives the value; a renamed folder is measured again under its new name and gone under the old. `#[ignore]` timing on a generated 500,000-file tree, and `heap_bytes` on a generated 1,000,000-folder tree against §3.6. The drive-type rules of §3.2 are a separate small function with its own tests (a `mountinfo` text, a `mount` output, a UNC path).
**R14.2 (O) Watcher.** Owns `sizes/watch.rs` (new), `Cargo.toml` (`notify = "8.2"`, after decision 29). §3.3. Tests: create, write, rename and delete each produce the holding folder within 2 s; 10,000 quick changes become one batch or `Rescan`; 10,000 changes in folders `wanted` refuses produce no batch at all; dropping the watcher stops its thread. The Linux watch limit behind `cfg(target_os = "linux")` with its own test.
**R14.3 (S) Live rows and host sizes in the app.** Owns `state/sizes.rs`, `state/listing.rs` (merge by name), `state/loader.rs`. Tests: a file made outside the app appears within 1 s with selection and scroll unchanged; a selected file deleted outside leaves the selection; `watch_changes` off stops both. Screenshot: Running, Exact and "over" cells in one list.

**R15.1 (S) Storage bar.** Owns `components/storage_bar.rs` (new), `components/disk_strip.rs` (keep `layout`, remove its drawing), `views/explorer.rs` (placement), `components/sidebar.rs` (the 4 px bar). §4 in full with `MIN_PARTITION_PX` and `MIN_SEGMENT_PX`. Layout is a pure function from (partitions, `ClassBytes`, width) to rectangles: test it (fixed order; small kinds join Other; a 100 MB partition on a 1 TB disk gets 28 px; widths sum to the bar). Screenshots: measured, measuring at about half, an unreadable partition, a selection's measure, compact; both themes.
**R15.2 (S) Layout mode.** In §10's order the disk view (R7) does not exist yet when R15 runs, so this task is done **with R7**, not with R15.1: it owns the switch in R7's disk view ("Contents | Layout", a segmented control over the large bar) and draws Layout with the existing `state/strip.rs` code, unchanged. Until then the relief drawing has no place on screen, and "Show where it is on the disk" stays out of the menus (`Built::layout_mode`). Screenshot: a file's pieces in Layout, both themes.

**R16.1 (S) The rule in the app.** Owns `crates/app/src/state/visibility.rs` (new), `state/listing.rs`, `state/search.rs`, `components/statusbar.rs` ("4 hidden"), `extract/mod.rs` (`copy_system_items`). §5.1 from "Two switches" on; the table itself exists since R12.0. Tests: the two switches are independent; a search does not return a file inside `.Spotlight-V100` unless system items are shown; `.hidden` at the root hides the names it lists; sizes still count hidden bytes; the status bar's temporary reveal ends on navigation; copy-out leaves `.DS_Store` behind and copies it when selected by name.

**R17**, in this order (1 to 6 are R17a; 7 and 8 are R17b, after Q1); each parser with a fuzz target in `fuzz/fuzz_targets/` and seeds:
1. **R17.1 (S) Sidecars.** Owns `mac/appledouble.rs` (new), `fuzz/fuzz_targets/appledouble.rs`, and the join in `state/facts.rs` and `state/quick_look.rs`. §5.2's first row and §5.3 with `MAX_AD_ENTRIES` and `MAX_AD_HEADER`. Behaviour: only when `VolumeInfo::resource_forks` is false and `merge_sidecars` is on; one `lookup` of `._name` for the file being shown, never one per row; a sidecar whose magic or version is wrong is treated as an ordinary file. Tests: a sidecar made by the existing writer in `extract/appledouble.rs` reads back to the same Finder information and fork bytes; an AppleSingle file; hostile ones (an offset past the end, offset + length overflowing `u32`, 65 entries, a truncated header); a FAT-like test volume with `Notes` and `._Notes` shows the fork switch, the type and the label on `Notes`; with the setting off it does not. Fuzz: 60 s clean.
2. **R17.2 (S) Property lists and attributes.** Owns `mac/bplist.rs`, `mac/xattr.rs` (new), `fuzz/fuzz_targets/bplist.rs`, and the Mac information loader in `state/facts.rs`. §5.3 with every bound, and the marker table. Tests: the three real files named there; an array that holds itself (the visited set ends it); depth 33; a count that overflows the file; a `0x1n` count marker with n = 4; quarantine text with three and with four fields, and with a non-hex time; a tags list with and without the colour line. Properties shows "Where from" and "Downloaded" for a fixture file carrying both attributes.
3. **R17.3 (S) Packages and labels.** Owns `mac/bundle.rs` (new), `components/file_row.rs`, `state/listing.rs` (Enter on a package), `state/menus.rs` ("Show package contents"), the index build's class for a package. §5.4, first and third items. Tests: each extension in the list and the bundle bit make a package; a file named `x.app` does not; `packages_as_folders` turns it all off; Enter on a package opens Quick Look and the menu item goes inside; the breadcrumb then shows the package as a folder; label 0 draws no dot, 1 to 7 draw the right colour, and the classic names appear in Properties on an HFS volume. Screenshot: a folder with two packages and four labelled files, both themes. (A package's own icon is task 7.)
4. **R17.4 (S) Trash and Desktop rows.** Owns `mac/trash.rs` (new), `fuzz/fuzz_targets/trash_info.rs`, `state/sources.rs` (detection), `components/sidebar.rs`, `state/listing.rs` (the "Was in" column). §5.2's Trash and Desktop rows. Detection runs once per mount on the background executor: `lookup` of `.Trashes`, `.Trash`, `Trash`, `$RECYCLE.BIN`, `.Trash-1000` and `Desktop Folder` on the root, then one listing page of each to see whether it holds anything. Tests: each kind of trash on a fixture; one user goes straight in, two show "User 501" and "User 502"; an empty trash gives no row; `$I` versions 1 and 2, a count larger than the file, a path with no closing zero; `.trashinfo` with percent-encoding and with a missing `Path=`. "Was in" is empty for Mac trashes until task 6 lands; say so in the report. Turns on `Built::trash_rows`.
5. **R17.5 (O) Resource maps and aliases.** Owns `mac/resource_map.rs`, `mac/alias.rs` (new), `fuzz/fuzz_targets/resource_map.rs` and `alias_record.rs`, "Show original" in `state/navigation.rs` and `state/menus.rs`. §5.3's two tables and §5.4's second item with `MAX_LINK_STEPS`. Verify first: tags 14, 15, 18 and 19, and version 3, on an alias made by Mac OS X; if no such file is at hand, build version 2 only and report the gap. Tests: a map built by hand with two types and a named resource; counts stored as `0xFFFF`; a reference list that points outside the map; overlapping data and map; a resource longer than `max`; the alias table field by field; odd-length tags and their pad byte; a missing end tag; a link whose target is itself (64 steps, then an error in words); a target on another volume gives the sentence of §5.4. Turns on `Built::show_original`. Q1 reuses `resource_map.rs` for icons and fonts: its interface is fixed here.
6. **R17.6 (O, then S) `.DS_Store`.** O owns `mac/ds_store.rs` (new) and `fuzz/fuzz_targets/ds_store.rs`; S then owns the uses: comments in `state/facts.rs`, "Was in" for Mac trashes in `state/listing.rs`, `finder_views` in `state/navigation.rs`. §5.3 with every bound. Verify first: `ptbL` and `ptbN` on a real Trash folder's file. Tests (O): two real files, one from a disk image with a background picture; a block address past the file; a tree with a cycle (the visited set); depth 17; 100,001 records; a record whose type does not fit its code is skipped and the rest are kept; names compared without regard to case. Tests (S): a comment shows in Properties; with `finder_views` on, a folder whose file says list view sorted by date opens that way, and the user's own change wins for the session; with it off, nothing changes.
7. **R17.7 (S) Custom icons.** Owns the icon sources in `state/thumbnails.rs` (R6) and `mac/bundle.rs` (`Info.plist`). `.VolumeIcon.icns` for the volume, `Icon\r` for a folder with the custom-icon flag (its `icns` resource, id −16455, through `resource_map.rs`), and a package's `CFBundleIconFile` (§5.4). After R6 and Q1's ICNS reader. Each goes through the thumbnail pipeline's limits and cache; a file that fails to decode falls back to the kind's glyph without a message. Tests: each of the three sources on a fixture; an `Info.plist` of 2 MiB is refused; an icon file name with `..` in it is refused. Screenshot: the sidebar and a list with real icons, both themes.
8. **R17.8 (S) "As arranged".** Owns `views/arranged.rs` (new) and the chip in the icon view. §5.5 with `MAX_ARRANGED` and every clamp. After R6. Tests: places are clamped; an item without a place flows below; a background picture outside the volume, or larger than 4096 × 4096, is not drawn; nothing can be dragged; the three values of `finder_layout`. Screenshot: an installer disk image, as arranged, both themes. Droppable (decision 35): if it is dropped, say so under "Changes during build" and remove `finder_layout` from the settings table.

**R5, added to R1's milestone: (S) the shared window.** Owns `views/aux_window.rs` (new), `keymap.rs` (`AuxWindow`, `aux::Close`). §7.2. Quick Look's window is built on it. Tests: bounds saved off-screen come back on a display that exists; Escape and Ctrl+W close; closing the main window closes it; with `cascade`, a second window opens 24 px down and right.

**How a line above becomes a brief.** The lead fills the template in `orchestration.md` and pastes, not points. R13.1 worked through, as the pattern for the rest:

```
Goal: Build IndexSizes, the table that gives every folder on a volume Disk Atlas
  reads its total size and gives the volume its bytes per kind. The Size column,
  the storage bar, Properties and the space map all read it.
You own: crates/lib/src/sizes/mod.rs (new), crates/lib/src/sizes/index.rs (new),
  the `pub mod sizes;` line in crates/lib/src/lib.rs. Nothing else.
Interfaces: <paste the Rust block of r10-everyday.md §3.1, whole>
  <paste the signatures you call: Index::len, parent, size, rsize, flags, node,
   subtree, from search/index.rs, with today's line numbers>
  <paste mac::conventions::system_item and SystemItem::HfsPrivate>
Read first: r10-everyday.md §3.1 from "What the two numbers mean" to "Cost";
  search/index.rs lines 16 to 31 (the layout); search/build.rs lines 20 to 30
  (folders reported twice); .claude/rules/engine.md.
Behaviour: <paste the six numbered steps and the overcommitted paragraph, with
  DENSE_NODE_LIMIT = 1 << 28 and MAX_LINK_NESTING = 64 word for word>
  The index may come from a saved file: every array access is get(), every sum
  saturating, every loop bounded by the entry count. No panic on any input.
Tests: <paste the test list of R13.1>
Done means: cargo test -p disk-atlas-lib sizes:: passes; cargo clippy -p
  disk-atlas-lib --all-targets -- -D warnings is clean; the two #[ignore]
  timings run with --release -- --ignored --nocapture and print.
Out of scope: HostSizes, the watcher, anything in crates/app, changing Index.
Report: the two timings against 150 ms and 250 ms, heap_bytes for the
  1,000,000-entry index, and anything in §3.1 that did not fit the code.
```

**R18.1 (S) Facts in one place.** Owns `state/facts.rs` (new), `components/details.rs`, `views/preview.rs` (Details tab): move the builders, no visible change, tests stay green.
**R18.2 (S) The window.** Owns `views/properties.rs` (new), `state/properties.rs` (new), `keymap.rs` (Alt+Enter, Ctrl+I, Ctrl+Shift+I). §7 in full with `MAX_PROPERTIES_WINDOWS`. Tests: the right sections for a file on HFS+, a folder, a host file, a volume, a disk, several items; a second request focuses the first window; results for a closed window are dropped; "Copy everything" text. Screenshots: a classic Mac file with type, creator and label; a folder with What is inside; a volume; both themes.
**R18.3 (S) Checksum** (after decision 9).

**R10b.1 (S) Settings window.** Owns `views/settings.rs` (new), `keymap.rs` (Ctrl+,). §8.2 and §8.3, rendered from `SETTINGS`. Tests: every row renders for every `SettingKind`; search finds a row by a word of its help line; a change is visible in the main window without restarting (theme, density, `show_system_items`); Reset restores the default. Screenshots: Browsing and Folder sizes, a search with matches in two sections, both themes.

## 11. Decisions that are Julian's

Numbered after R1's 17 to 28. **Julian approved all of 29 to 42 on 2026-11-13**; the third column is what was approved. Two of them stand on decisions of other files that have their own status: 37 on decision 14 (the cache folder) and 40 on decision 9 (`sha2`).

| # | Decision | Approved |
|---|---|---|
| 29 | A new dependency, `notify` 8.2 (CC0-1.0, released 2026-11-08, 58 M downloads in 90 days; 2 new crates on Windows, 7 across all systems), to watch host folders | Yes. The alternative is our own `ReadDirectoryChangesW`, FSEvents and inotify code: three sets of `unsafe` for no gain |
| 30 | Which host folders are measured by default | The ones you open, finishing in the background. Whole drives only when asked |
| 31 | A disk's file list is read when the disk is opened, not at the first search | Yes: folder sizes, the storage bar and the space map all need it. It can be turned off |
| 32 | The storage bar replaces the scale strip; the physical layout moves to the disk view | Yes |
| 33 | Housekeeping files are hidden on every system behind their own switch, Windows and Linux ones included | Yes, the table in §5.1 |
| 34 | Copying a folder out leaves housekeeping files behind unless they are shown or picked by name | Yes |
| 35 | `.DS_Store` is read (comments, put-back, optional folder views), and "As arranged" is built for disk images | Yes to reading. "As arranged" is the first thing to drop if R17 runs long |
| 36 | "Properties" on every system; Alt+Enter and Ctrl+I open it; the details pane moves to Ctrl+Shift+I | Yes |
| 37 | "Open a copy in its app" writes a temporary copy into the app's cache folder and removes it on quit | Yes (decision 14 already allows the cache folder) |
| 38 | Settings is its own window and its store is built now, not in M8 | Yes |
| 39 | Sizes are counted the way each system counts them (1024 on Windows, 1000 on macOS and Linux), with a setting | Yes. Today everything is 1024 and labelled GB |
| 40 | SHA-256 in Properties through `sha2` | Yes, with decision 9 |
| 41 | No Eject, no Windows shell menu, no editing of permissions | Agreed as limits of this plan |
| 42 | A folder's size counts a hard-linked file once for each name (what copying it out gives), while the storage bar counts each file once. On a Time Machine disk a backup folder therefore shows its full size, and the folders together add up to more than the disk | Yes. The other choice (count once everywhere) makes every backup after the first look almost empty |

Fable decided, for the record (README rule 4 cases that are the architect's): the saved index goes to version 2 with reserved bits given meanings; `ScanEntry` gains `finder_type`; `EntryFlags` gains package, alias and label; a new engine module `mac/` for Mac conventions and `sizes/` for size tables; the engine may own worker threads for `HostSizes`.

## 12. Out of scope

Reading Spotlight's store, the FSEvents log or document revisions. Browsing Time Machine backups as a timeline. Classic Finder comments from `Desktop DB`. `desktop.ini` names and icons. Modern bookmark aliases. Rebinding keys. Measuring a host NTFS drive from its file table. Allocated ("on disk") size for folders. Battery-aware throttling (no safe API). Anything that writes to a disk Disk Atlas reads.

## 13. Review, 2026-11-13 (Fable, second pass)

The first draft was read again against the code and the GPUI source, and the facts it had left unread were read. What changed:

| Found | Change |
|---|---|
| A linked folder has no children in the index: its contents hang under the HFS+ private store (`search/build.rs:23`). The first draft would have shown every folder of a Time Machine disk as empty, and classed hard-linked files by the name `iNode<n>` | §3.1 rewritten: what the two numbers mean, the private stores, the directory-link step, decision 42 |
| The first draft's forward pass used each package's `end` before the pass that computed it | `end` has its own pass first |
| `Totals::folders` had no storage, and 16 bytes for every entry was more than needed | The table is kept per folder: about 4 MB for a million entries instead of 16 |
| Housekeeping was a flag on the item only, so a file inside `.Spotlight-V100` would have been a search hit and a "Document" in the bar | Bit 6 is passed down to everything inside (§2, §5.1) |
| The index would have been saved as version 2 in R12 without bit 6, which the first draft set in R16 | The table moves to R12.0; R16 keeps the app's rule |
| A watcher on a system drive hears thousands of changes a second in folders nobody measured; the first draft counted them toward the limit that forces a full re-measure | `wanted` drops them first (§3.3) |
| The bar had no state for a host drive that was never measured, which is the default | "See what fills this drive" (§4) |
| The space map and "What is inside" could not list a host folder's sub-folders | `HostSizes::children`, `own`, `knows` |
| Custom icons were ordered before the ICNS reader they need | R17 split into R17a and R17b |
| The app cannot tell a network or removable host drive | §3.2 says how, without `unsafe` |
| `HostSizes` at 4,000,000 folders had no memory bound | 2,000,000 and a 96-byte budget per folder |
| The `.DS_Store` root block lacked its 4 unknown bytes, the padding of its address table and the directory's layout | Added, still marked "verify first" |
| Ctrl+click on macOS, the Menu key on Linux, where the index build starts, `notify`'s release facts | Read and recorded in §9 |

**After approval, the same day**, the spec was finished so a lead can brief from it without guessing: the types the first draft only named (`SourceFacts`, `RowFacts`, `MenuState`, `Built`, `Sizes`, `RowSize`, `HostDir`, `PropertiesTarget`, `Fact`, `AuxOptions`, `Section`, `SettingValue`, the bar's `layout`); the failure states of sizes (§3.5); the key table checked against `keymap.rs` (§6.3); the shared window (§7.2); the on-disk layouts checked against a published source each, with what is still from memory marked (§5.3, §9); the R17 briefs written out; one brief worked through in the `orchestration.md` template (§10.1). `roadmap.md`, `performance.md`, `architecture.md` and `ui-design.md` carry the matching entries.

Not changed, and looked at: the kinds table against R1's twelve tints (they match once `Model` is added); the settings keys against M8 §8.1 (no clash); the menu's use of `dispatch_action` (it is the same path as a key binding); the order of §10 against every "Depends on" cell.

## 14. Changes during build

2026-11-13, Phase B:

- **R10a:** `Settings` holds maps with typed getters, not one field per key; repeated keys and the `extension.*`/`hfs_encoding.*` families go through `set_list` and `set_dynamic`. `size_units` values: `system`, `decimal`, `binary`. `motion::is_reduced_now(cx, system_reduced)` is the one reader (env var overrides: on reduces, `0`/`false` forces full). A save in flight at quit can be lost (nothing joins the worker): fixed in R5 (`Settings::flush`, below).
- **R12.0:** `.PKInstallSandboxManager*` map to `TemporaryItems`, `.MobileBackups` to `TimeMachineMarker`, `.vol` to `HfsPrivate`.
- **R12.1:** `classify(name, finder_type)` never answers `Folder`; `classify_entry(name, is_folder, finder_type)` is what the app and the build call. Type codes `ZIP ` and `Mp3 ` carry the trailing space. `IndexFlags` bit 7 (package) is set for folders only; `EntryFlags::PACKAGE` stays raw on files (a classic file's bundle bit means "has BNDL"). `ALIAS` is not in the index; bit 15 is the last spare flag bit. Loader v2 also rejects a package bit on a non-folder and a child missing bit 6 under a parent that has it. Opus review done (second `opus-engineer`).
- **Open, for a real disk or Fable:** (a) a resolved hard link keeps the link record's Finder flags (`fs/hfsplus/scan.rs:374`); directory links probably carry `kIsAlias` and not the target's bundle bit or label, so packages in a Time Machine backup may miss bit 7 and show an alias badge. (b) A hard-linked folder's children hang from its first report; if that report sits inside a housekeeping folder the subtree gets bit 6 and the other report lists empty. (c) The HFS+ scan never reports the private folders, and the first report of a linked folder is a directory link, not `dir_<n>`: R13.1's private-store tests must be rechecked against this before the brief goes out. (d) `Index::verify` does not check the two new v2 rules; `index_build_throughput` is not re-run with `system_item` per entry; `fuzz/seeds/search_index_load` seeds are version 1 and need regenerating.

2026-11-13, Phase C:

- **R13.1:** `IndexSizes` stores `by_node` as two arrays (node, entry), 36 bytes per folder; `with_volume_total(Option<u64>)` sets `overcommitted`; added a link-resolution work budget (links x 64 + entries, then link totals stay at step 4 and the lower-bound flag is set). A private store, when present, is left out of the root completely, and everything inside it counts as `system` in `volume` (bit 6 is passed down), not only orphans. `classes_of` counts every name and does not follow directory links. Always resolve a row with `folder_entry(node)`, never a link's own entry number (a link's `children` and `classes_of` are empty). Later formats must give folders unique node numbers. Timings (release, median of 7): 13 ms for 1,000,001 entries, 29 ms with 100,000 directory links. Opus review: accepted, 2 tests added. Today's HFS+ scan reports no private store and hangs a linked folder's children on its first report (the link), which the tests cover.
- **R13.2:** `Sizes` entity builds the table; `Listing` keeps a `tables` map pushed in by `Sizes` (the entities depend on each other in a cycle); `RowSize` has `None` and `Fixed` only (Host is R14). Folder rows keep sparse totals, read by binary search at paint time. "Mouse button down" is approximated by "a click counts as input" (no mouse-up hook). F5 reloads the listing and then calls `recheck`, so a changed volume may reload twice. Search-result rows show no folder total.

2026-11-14, R15.1 (storage bar):

- **R15.1:** `components/storage_bar.rs` owns the bar and mounts in `app.rs` where `DiskStrip` was. It owns the `Contents | Layout` switch and, in Layout mode, renders the unchanged `DiskStrip` (`state/strip.rs` still serves it). `layout` and `legend` have the design's signatures; `legend` counts what no file accounts for as System, so the legend sums to the used bytes. Cursor rings use `ink` (the accent `pin` stays the selection's bracket and dot). `compact` hides the legend, `hidden` draws nothing.
- **Gap, host drives:** `NativeFs::info()` returns no `total_bytes` or `free_bytes`, and nothing else in the app knows a host drive's used and free space (the sidebar's "3.7 TB" is the physical disk). A host drive therefore draws no bar yet; the code path (used and free from `VolumeInfo`) is in place and starts drawing as soon as the host source reports them. "See what fills this drive" and "Measuring… N %" wait for R14 (`HostSizes`); the TODO is in `Model` building.
- **Not done:** right-click entries (the menu component is R17's), the Sizes-driven "Rebuild the index" entry, the 4 px sidebar bars were already there and are untouched. Segment widths and the "grow in" animation are not eased: the bar redraws at once; only the measuring sweep animates (with a still stripe under reduced motion).
- **Seen in the window (dark and light, `fuzz/seeds/hfsplus_mount/compressed` as an image):** bar, legend with System and Other chips, hover dimming, tooltip in words, click on a chip sets the Kind filter and Size sort, Layout switch shows the old strip. Clicking "Other" gave 0 results although the chip says 12 bytes: the M4 Kind filter's Other and the bar's Other (Other + Folder classes) may not match; check in R15.2.

2026-11-14, R15 leftovers:

- **Right-click and Menu key:** the bar uses `MenuSlot` from `components/menu.rs`. A partition (and a System or free piece, through the partition) offers "Open" and "Properties"; a kind segment or chip offers "Show the largest <Kind>" (the click's action) and "Show on the space map". "Properties" (R18) and "Show on the space map" (R7) are present and greyed with a reason, because neither exists yet. The Menu key and Shift+F10 open the menu of the stop the keyboard is on. The four actions are local to `storage_bar.rs` (`OpenPartition`, `PartitionProperties`, `ShowLargest`, `ShowOnSpaceMap`) and no key is bound to them. Tooltips are not attached while a menu is open (they covered its first item). The keyboard stop is drawn only after a key press, not after a click.
- **Other, decided:** the Other chip and segment stand for Other, Folder (when it has bytes) and every kind folded into Other at the current width, so a click searches that whole `KindSet` (`kinds_of`, same fold rule as `legend`). The bar counts hidden files, so a click from the bar also sets `include_hidden`: the results then add up to the chip's bytes (the 12-byte "Other" of the test image is three small hidden-in-the-sense-of-folded .txt files: top, mid and deep, 12 bytes). This changes the view option "Hidden items" as a side effect of the click; R16 may want to make that a one-search override instead.
- **Selection measure:** `Listing::selection_total` is `None` for files, so the bar sums selected files itself (`selection_measure`, up to 4096 rows); a single file's bracket starts at its kind's segment (Other when the kind was folded), anything else at the partition's left edge. The bracket row is always reserved so the bar does not jump when a selection appears.
- **Motion:** the kinds of a volume grow in from the left over EMPHASIZED once per volume per session (the free part takes what is left); later changes of the pieces of the same partition at the same width ease over STANDARD; resizing does not ease. Reduced motion draws everything at once. The legend's numbers are held for `LEGEND_INTERVAL = 250 ms` while a volume is being measured (no volume reaches that state yet: tables arrive in one step, so the hold is exercised only by R14's host measuring).
- **Seen in the window (`m.img`: a hand-made MBR image with two HFS+ partitions, one zeroed Linux partition and gaps; built in the scratchpad, not committed):** the multi-partition bar, a 28 px partition, an unreadable partition, unallocated outlines, the selection bracket and text, compact and hidden settings, the context menus, shift+F10, keyboard stops, dark and light. The animations were not seen (a 240 ms movement cannot be captured with the screenshot tool).

2026-11-14, R16 (hidden and system items in the app):

- **R16:** `state/visibility.rs` holds `Visibility` (the two switches, `shows(hidden, system)`) and `Rule` (the switches, `at_root`, `reveal`). A housekeeping item follows `show_system_items` alone, even when the volume also marks it hidden; anything else hidden follows `show_hidden`. The folder `Loader` takes the rule (`with_rule`), keeps filtered rows in `Rows` but out of the reported order, and reports how many it hides (`Step::hidden`); `sort_rows_visible` does the same for a re-sort. A change of either setting re-sorts the loaded folder (the selection is dropped); a folder put aside behind search results is brought up to date when the results are left. Sizes still read the whole `Rows`/index and are untouched. Quick Look's previous and next move the list's own selection, so they follow the filtered order without code. A shown housekeeping row is drawn at `SYSTEM_ITEM_OPACITY = 0.55` and its Kind says `SystemItem::label`. The View submenu of the folder menu has "Show hidden items" and "Show housekeeping files" (the settings' own titles); Ctrl+Shift+. is `workspace::ToggleHidden` (`secondary-shift-.`). The status bar says "17 items · 4 hidden"; the count is a button (focusable, Press) that reveals the folder until you navigate elsewhere (F5 keeps it), and a second press hides again; its tooltip names both settings.
- **Search:** a search starts with its "Hidden items" filter at `show_hidden` and follows the setting while it runs; `Filters` is reset when the search ends, so the R15 storage-bar click's `include_hidden: true` is now a one-search override and the setting is untouched.
- **Gaps (need a decision):** (a) `Query` has no `include_system`, so search still returns housekeeping items (and everything inside `.Spotlight-V100`) whatever `show_system_items` says; the index has bit 6, the engine's `admit` and `Query` need the field (a `lib` API change). The storage bar's chip bytes count system items, so a click from the bar will not add up to the chip until that lands. (b) `CopyOptions` has only `hidden`; there is no `copy_system_items`-style option in `extract`, and the app has no copy-out yet (M5), so the setting `copy_system_items` is read by nothing and "left behind unless selected by name" is untested. The engine's walk is where it belongs (`wanted`/`hidden_ok`, with `system_item`). (c) `.hidden` at the root is not read by the engine or the app: names it lists are not hidden. (d) "Folders first" is still not read by the list. (e) Ctrl+Shift+. could not be sent by `shot.ps1` (no "." key); the toggle through settings and the button were seen.

2026-11-14, R5 (frame done, platform calls pending):

- **R5:** `views/aux_window.rs` has `AuxOptions`, `open`, `close_window`, `close_all`, `register_main`, `window_closed`, `fit_to_displays`, `remembered`/`remember`. Extra to the design: `close_window` (every in-app close goes through it so the last resize is remembered), `register_main` + `window_closed` (the one `on_window_closed` rule in `main.rs`: forget closed aux windows, close the rest when the main window is gone, quit when none is left). Bounds are **session-only** (Julian's call for this task, although the store exists): `remembered`/`remember` are the two functions to change, marked `TODO(R10b)`; the keys `window.*` are already in the table. A window counts as on a display when at least 100 x 32 px of it overlaps one; otherwise it is centred on the first display; else clamped inside the display it overlaps most. Cascade: 24 px from the newest open window of the same key, size remembered, first window centred. `aux::Close` is bound to Escape and `secondary-w` in `AuxWindow`; the deeper `QuickLook` context wins for Escape (read in `keymap.rs:165`, `bindings_for_input` sorts deepest first), but no test can tell which of the two fired, because both end in a closed window.
- **Quick Look is now its own window**, 840 x 560, minimum 480 x 360. `QuickLookWindow` (in `preview.rs`) lives on `Workspace`: `sync` opens or closes the window when `QuickLook::is_open` changes, `vanished` tells the workspace the window closed some other way, and the workspace then closes `QuickLook` through the main window and focuses the list. One close route for Escape, Space, the panel's close button, Ctrl+W, the frame's button and Alt+F4. The panel root owns focus (`Focusable`), handles Close, the four file moves, tab and Go-to-byte actions and Tab/Shift+Tab itself, because the window has no `Workspace`. Removed: the overlay, `panel_size`, the scale of the fade (an opacity fade remains), `QUICK_LOOK_SHARE`, `QUICK_LOOK_START_SCALE`. `on_window_closed` runs inside the close, possibly inside a `Workspace` update, so `vanished` is called from `cx.defer`.
- **Not done, `TODO(R5-platform)`:** keep on top (Windows `SetWindowPos`, macOS `setLevel`; needs `unsafe`/`objc` and Julian's approval), the full screen button and F11 / Ctrl+Cmd+F (GPUI has `toggle_fullscreen`), Linux (no server-side bar design; not compiled here). Not done and not asked: the window title does not follow the file name; Left/Right neighbours and the toolbar buttons are R6.
- **Settings flush (the R10a gap):** `Settings::flush()` waits up to 2 s on a `Condvar` for the save worker and writes a waiting job itself if no worker ever started; registered with `cx.on_app_quit` in `main.rs`. One writer at a time, so the temporary file is never shared.
- **Seen in the window (Windows, dark theme only, debug build, `tmp_vite.log` selected):** Space opens the Quick Look window with the frame (title "Quick Look", bar, close button) over the panel; Escape closes it; Space opens it again; Ctrl+W closes it; the main window stays open each time; closing the main window with the Quick Look window open ended the process (so the Quick Look window closed with it). Light theme, a second display, DPI changes and macOS/Linux were not seen. Not checked: whether the new window takes OS focus by itself on Windows (`WindowParams::focus` is unused there, `platform.rs:1169`; the screenshot tool brings a window forward before sending keys, so it hides this), and Space in the main list closing the open window.
- **Looks to settle later:** the window now shows two close buttons (the frame's and the panel header's) and two titles (frame "Quick Look", header file name). R6's toolbar redo of the header is the place to drop one.

2026-11-14, R10b (settings window):

- **R10b:** `views/settings.rs` renders every row from `SETTINGS` (switch, segmented for 2 to 4 options, menu above 4, stepper, Hidden never shown), with search over label, help and option names (section list shows counts), the dot and Reset on rows off their default, "Reset this section", Keyboard (a hand-kept static list of what `keymap.rs` binds, not read from the keymap), Advanced and About. One instance: a second request focuses it. Ctrl+, is `workspace::OpenSettings`. Debug builds: `DISK_ATLAS_OPEN_SETTINGS=<section>[:<search>]` (`views::settings::open_from_env`, called in `main.rs`) opens it for screenshots. `aux_window::remembered`/`remember` now read and write `window.*` as `x,y,width,height` in whole pixels (bad text or absurd values give `None`); the session-only global is gone. `Settings::is_default` and `Settings::file_path` added.
- **Theme is now live:** nothing read the `theme` setting. `theme::resolve` / `theme::apply_setting` apply it over the system appearance at startup, on a system change and on every `Settings` change (observer in `Workspace::new`). `density`, `window_material`, `label_dots` and others are still read by nothing: the window writes them, the app ignores them until their milestones.
- **Not done:** no "Settings…" menu item (there is no top-bar menu; `state/menus.rs` holds context menus only; adding it to the folder menu would be a design call). "Remove the permission to read disks" (R9), "Delete saved indexes", Extensions (M8), greyed rows for "needs a newer system", switch animation. The search field is a small key-down field (no caret movement, selection or IME). "Open the settings file" reveals it in the file manager. Window and row sizes are constants in `settings.rs`, not in `theme::metrics`. Section glyphs reuse the kind-legend glyphs.
- **Seen in the window (Windows, debug build):** dark: Browsing, Folder sizes, Appearance (dot and Reset after choosing Day), a search ("hou") matching three sections with counts. Light: choosing Day in the window switched the whole window at once; Browsing and a search with matches in three sections. **Not seen:** a light Folder sizes shot (not taken), a menu above 4 options, window bounds after a restart, Tab order, the main window in light. The screenshot tool's `type` did not reach the search field (`key` per letter did). `shot.ps1` fails under `pwsh` 7: use `powershell.exe`. Its `-Title Settings` also matches the Windows Settings app (the largest wins), so Disk Atlas's Settings window was shot by launching it larger (a `window.settings` line in the settings file).

2026-11-14, R14.1 and R14.2 (host sizes, watcher):

- **R14.1:** `sizes/host.rs` as §3.2, reviewed; 29 tests on temporary trees. Release timings (in `performance.md`, "Host sizes and the watcher"): `heap_bytes` 82.7 MiB for 1,000,000 folders (budget 96), the 500,000-file walk 213 ms to `Exact` (budget 3 s).
- **R14.2:** `sizes/watch.rs` as §3.3. `Watcher::focus(&[PathBuf])` added: on Linux it replaces the extra watches with the first `MAX_LINUX_WATCHES` folders given (nearest the open folder first), elsewhere it does nothing. The root is watched recursively on Windows and macOS, on its own on Linux. Access events are ignored; an error delivered by `notify` becomes `Rescan` like the rescan flag; a panicking `wanted` counts as a refusal and a panicking `sink` is caught. Paths reported under the canonical root (macOS `/private/var`) are moved back under the caller's spelling. Drop stops `notify`, waits out a `wanted` call in progress, then joins the flush thread (not when dropped from inside `sink`). No new error variant: `notify` errors map onto `Error::Io` with `IoContext::Path { op: "watching" }`. "10,000 quick changes become one batch" is tested as at most one batch per `WATCH_FLUSH` while writing plus one, since the writes take longer than one flush. 21 tests on Windows; the Linux `focus` test is behind `cfg(target_os = "linux")` and was not run here (macOS compiles).
- **Found, Windows:** `notify` 8.2.2 never sets the rescan flag on Windows. A probe of 16,000 creates with long names in one folder (well past the 16 KiB change buffer) gave twelve small `Dirs` batches and no `Rescan`. §3.3's "Windows: the change buffer overflowed becomes `Rescan`" therefore never happens, and overflowed events are lost without notice. **Decided for R14.3:** on Windows the app also applies `REVALIDATE_AFTER` to watched folders (a folder shown again whose numbers are older than 60 s is re-measured), the same backstop §3.5 gives unwatched drives. If this is not enough, it is a question for Fable (a per-flush sanity check, or a different backend call).
- **Stop and ask (Fable), budget missed by more than 2x:** "One watcher flush of 1,000 changed folders, ≤ 50 ms on a worker" measured 140 ms (release, Windows NTFS on NVMe, 1,000 `HostSizes::invalidate` calls in a row). The watcher's own work is not the cost: a bare `read_dir` + `metadata` loop over the same tree takes 70 to 104 ms, so 1,000 folders read one after another cannot meet 50 ms on this platform. The choices are Fable's: spread a batch's `invalidate` calls over the size workers (an engine-internal change; the API stays), or restate the budget (per platform, or per folder). R14.3 does not depend on the answer: it calls `invalidate` from a worker either way.
- **Answered (Fable, 2026-11-14): the budget is restated, the engine is not changed.** New budget: 200 µs or less per changed folder, so 200 ms or less for the 1,000-folder flush, the same on every platform, 0 on the UI thread (§3.3, §3.6, `performance.md`). The 140 ms measured is inside it; it is one sample, so the figure is confirmed with a median of 3 the next time the timing tests are run. `HostSizes::invalidate` keeps its signature and its behaviour (it reads on the calling thread). Not taken: spreading a batch over the size workers; the reasons and the condition for looking again are in §3.3. Two things follow for the lead, neither a design question: (1) R14.3 applies batches as §3.3's "Batches never pile up" says: drain, merge, and one `invalidate_all` past `MAX_BATCH_DIRS`. (2) The timing test `one_flush_of_1000_folders_into_invalidate` (`sizes/watch.rs`) still prints "budget 50 ms": change it to 200 ms, take the median of 3, and have it print the bare `read_dir` and `metadata` loop over the same tree beside the result, so a later miss can be told apart from a slow disk (H, with R14.3). Linux and macOS are not measured yet; the same 200 ms applies there and is expected to be met with room.

2026-11-14, R14.3 (host sizes and live rows in the app):

- **R14.3:** `state/sizes.rs` holds the host half. `Sizes` keeps `hosts: HashMap<SourceId, HostSlot>` (`Creating`, `Skipped`, `Ready(HostTable)`) next to the index `tables`; there is no `Table` enum, because the index tables are `Built` structs with more in them. `RowSize::Live(SourceId, HostDir)` is read through `Rows::size_state` (one short tree lock per drawn folder row, no path resolved in a frame); there is no `Sizes::state(row)`, since the Explorer never receives the `Sizes` entity. The app's `SizeState` gains `Running` ("3.0 GB…" in `ink3`, no tooltip). `Sizes` follows the listing with `observe` (key: source, node, `load_epoch`), not with a `ListingEvent` variant, because explorer.rs and search.rs match that enum exhaustively. Constants: `SIZE_REPAINT` 250 ms, `LIST_REFRESH` 200 ms, `VISIBLE_SETTLE` 60 ms, `VISIBLE_MAX` 256.
- **The batch thread (`disk-atlas-batches`)** is the one thread that applies watcher batches, as §3.3 "Batches never pile up": it takes every batch waiting, merges per source (`merge_batches`, pure and tested: listed folder first, duplicates collapsed, exactly 4,096 stays `Dirs`, 4,097 or any `Rescan` is one `invalidate_all`), and tells the foreground as soon as the listed folder is done. Each source's pass is under `catch_unwind`. `invalidate`, `invalidate_all`, `request`, `refocus`, and creating and dropping trees and watchers all run on that thread or the background executor, never the UI thread. The UI thread takes only short locks (`state_of`, the stamp map, the `listed` path).
- **`REVALIDATE_AFTER`** is applied on every host source, watched or not (`Validated`, `plan`): showing a folder whose numbers are older than 60 s calls `invalidate` on it from a worker. A folder with no stamp is judged by when the tree was created. This is the §14 decision for Windows, where `notify` never reports overflow.
- **Merge by name (`Listing::reload_in_place`, `merge_by_name`):** a second path next to `load(focus)`, on purpose: `load` pushes pages into the visible list, which a refresh must not. The folder is read to its end on the background executor, then swapped in; selection and cursor stay on their names, a vanished selected name leaves the selection, a vanished cursor goes to the next row, then the previous. The anchor of a range selection becomes the cursor after a merge (`Selection` has no setter). A change that arrives during a load is merged when the load ends. The merge runs on the UI thread at apply time: O(rows), one hash set; a 1,000,000-row folder may cost tens of milliseconds once per outside change.
- **Not like the design, and why:** a network drive, or a drive whose type cannot be told, gets no tree and no watcher while `folder_sizes_network` is off (F5 still works). Removable drives are not detected; only the network rule (`is_network_drive`) applies. Whole-drives mode queues the root at `Background`, then the shown folder at `Listed`, and never calls `refocus`.
- **Lead's changes:** `Explorer::render_rows` defers one `listing.note_visible(range)` call per paint (the 60 ms settle is in `Listing`); the five `expect(dead_code)` marks that waited for it are gone. `theme::metrics::COLUMN_SIZE` 72 to 96 px: "over 10.8 MB" was clipped at the left ("ver 10.8 MB") in the window; 96 px fits "over 999.9 GB" and "1023.9 GB…". `performance.md`: the 1,000-folder flush is 95.1 µs per folder (median of 3, 95.1 ms for 1,000, budget 200 ms), baseline 69.0 ms; the timing test prints both (haiku-runner).
- **Gaps:** (a) The status-bar tooltip "Changes on this drive are not watched. F5 refreshes." is not shown: the state exists (`WatcherSlot::Failed`, logged once per drive), `statusbar.rs` does not read it. (b) "N folders inside could not be read": `Totals` has no such count, so an "over" cell keeps the lower-bound tooltip. (c) The scroll offset stays in pixels across a merge, so rows inserted above the viewport shift the names under it: keeping it on a name needs the Explorer. (d) `selection_total` ignores live host totals. (e) A host drive still draws no used/free bar: `NativeFs::info()` has no `total_bytes`/`free_bytes` (R15's gap, not R14.3's). (f) Not run: the Linux `Watcher::focus` path through the app, and `WatchBatch::Rescan` through a real watcher (covered through `merge_batches` only).
- **Open for Fable (not acted on): leaving a folder does not free the size workers.** `HostSizes::refocus` prunes only the `Background` stack. A folder's rows are requested at `Visible` priority, so after you leave it, its unfinished `Visible` walk (and the children it queues, which inherit `Visible`) stays queued, and the next folder's rows compete with it. Seen on this machine (debug build): open `X:`, whose `Documents` is about 1.6 TB, then open a small tree `da_shot`: its `big` folder (1,500 folders, 90,000 files, 879 KB) stayed at "1 KB…" for more than 16 s and was `Exact` about 30 s after opening, while the walk of `X:` ran on. The design says a left folder's unfinished background work is dropped (§3.2), but the engine drops only `Background`. The choices are Fable's: let `refocus` also drop `Visible` and `Listed` work outside `keep`, or give the app a call that demotes it. R14 is ticked without it.
- **Latency (Fable's "look again" condition, §3.3):** on Windows NTFS, a file written inside a listed child folder took 1.13 s to change that child's size (one run, debug build), above the 1 s that §3.3 names. The watcher's 500 ms flush is most of it; a second flush for the file's modify event is plausible and was not traced. Listed-folder rows took 0.92 s to show a new file.
- **Seen in the window (Windows, debug build, `X:` and a scratch tree `X:\da_shot`):** dark: `X:` with "159.8 GB…", "69.1 GB…", "10.0 MB…" (Running) beside "1.2 GB" and "14.6 GB" (Exact), later "1.6 TB…"; `da_shot` with `big` "1 KB…" (Running), `locked` "over 264 KB" (a folder with an access-denied sub-folder), `medium` 9.5 MB and `small` 195 KB (Exact); `big` ended at 879 KB. Light: the same list after the column fix ("176 KB…", "over 264 KB", 9.5 MB, 195 KB) and `X:` with "972.9 GB…". Home showed folder totals (14.0 GB, 3.0 GB, ...) arriving without a click. **Not seen:** a merge after an outside change in the window (tested only), Linux and macOS, the status-bar tooltip, a removable or network drive. `X:\da_shot` (about 11 MB of test files, one folder with a deny rule removed) is still on the drive: removing it was blocked in this session; delete it by hand.

2026-11-15, R17a (lane C):

- **R17.1:** the join is `mac::sidecar_fs::SidecarFs`, a `FileSystem` wrapper added in `Sources::add_disk` when `merge_sidecars` is on and the volume has no forks, not code in `facts.rs` and `quick_look.rs`. It remembers each entry's folder and name while a folder is listed, reads `._name` only when `metadata` or `open(Resource)` asks about that one file, and reports `resource_forks: true`, so the fork switch, Properties, the bytes view and copy-out all see the merged fork with no app change. A volume mounted before the setting changes keeps its old choice. Host folders are not wrapped.
- **R17.2:** `bplist::parse` returns `image::plist::Value` (the XML reader's tree), not a second tree; dates become ISO text. Properties shows the Mac attributes first in its Extended attributes section (`facts::xattrs_with_mac`).
- **R17.3:** double-click on a package does nothing (Enter, Space and the menu are as designed). The index build was not changed. Classic label names in Properties are not added.
- **R17.4:** `find_trash` runs in `mount_volume` on the background executor and `Sources::trash(id)` hands the rows to the sidebar, which opens them with `Location::root(source).child(node, label)`, so the breadcrumb skips `.Trashes`. The "Was in" column is not built.
- **R17.5:** `mac::original::resolve` does the walk. An alias is found by its path first and by its file number second (the folder is then known by number only and is called "Original's folder"). Version 3 and bookmark records say "This alias uses a newer format".
- **R17.6:** `ds_store::read_store(fs, dir)`; the S half is only the comment in Properties. `finder_views` and "Was in" for Mac trashes are not built. The reader was written from the notes and tested on files made by the test builder only.

