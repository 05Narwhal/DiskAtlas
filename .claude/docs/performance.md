# Performance

Speed is the product. This file sets the budgets and the designs chosen to meet them. The budgets are targets until a benchmark exists; once it does, the benchmark is the truth and this file is updated to match.

## Budgets

Reference machine: a mid-range 8-core desktop, volume on a USB 3 SSD.

| Interaction | Target |
|---|---|
| Launch to an interactive window | under 300 ms |
| Open a folder: first screen of rows painted | under 50 ms |
| Open a folder with 100,000 entries: fully listed | under 500 ms, scrolling usable from the first frame |
| Scroll and keyboard navigation | every frame within the display's refresh interval |
| Search: keystroke to first results, 1 million names | under 16 ms |
| Search: same, 5 million names | under 50 ms |
| Build the search index, 1 million entries, HFS+ | under 5 s |
| Reopen a volume with a saved index | index usable in under 200 ms |
| Quick Look on a text or image file | under 100 ms to first content |

Memory: the search index stays under about 80 bytes per entry including names. The block cache has a fixed byte budget (default 256 MB, configurable).

### Budgets added on 2026-11-13 (not measured yet)

Release builds, measured the M3 way (an `#[ignore]` test that prints, or a traced timing), and recorded under "Measured" by the task that builds each. A miss by more than 2x after the obvious fixes is a stop-and-ask.

| What | Target | Design | Built in |
|---|---|---|---|
| Size table from the index, 1,000,000 entries | 150 ms or less; 250 ms with 100,000 directory links | `r10-everyday.md` §3.1 | R13 |
| Size table memory | about 36 bytes per folder, nothing per file | §3.1 | R13 |
| Bytes per kind inside a 100,000-entry folder | 2 ms or less, off the UI thread | §3.1 | R13 |
| Host folder walk, warm cache, NVMe, 500,000 files | 3 s or less to exact | §3.2 | R14 |
| Host size tree memory, 1,000,000 folders | 96 MiB or less | §3.2 | R14 |
| Size cells in a frame of 60 visible rows | 0.2 ms or less; no frame over 16.7 ms while sizes arrive | §3.4 | R13, R14 |
| One watcher flush of 1,000 changed folders | 200 ms or less on one background thread (200 µs per folder), nothing on the UI thread. Restated from 50 ms on 2026-11-14 | §3.3 | R14 |
| Idle cost of a watcher on a busy system drive | under 1 % of one core, averaged over a minute | §3.3 | R14 |
| Any animation, on a 100,000-entry folder | no frame over 16.7 ms (`--profile profiling` trace) | `r1-redesign.md` §2.4 | R4 (measured: "Motion frame traces" below) |
| Space map layout | 8 ms or less; zoom at 60 fps | `r1` §3.5 | R7 |
| First visible thumbnails after scrolling stops | 150 ms or less; no frame over 16.7 ms while they arrive | `r1` §1.7, §8 | R6 |
| 1,000,000-triangle binary STL | parsed and welded in 400 ms or less; first frame 500 ms or less after Space | `r1` §7.2 | Q1, G1 |
| Turning a 20,000,000-triangle model, 2560 × 1440, mid-range discrete GPU | 60 fps or more (5,000,000 on integrated graphics) | `r1` §7.2 | G1 |
| UI thread per frame for the 3D viewer | 2 ms or less | `r1` §7.2 | G1 |

## Block layer

- Positional reads only (`read_at` on Unix, `seek_read` on Windows). No shared cursor, so any number of threads read at once.
- A chunk cache (64 KB aligned chunks keyed by source and chunk number, sharded, CLOCK or LRU eviction, byte-budgeted) sits under every filesystem. Raw devices need sector-aligned reads; the cache's alignment provides that for free.
- Read-ahead: when access to a source is sequential, fetch the next chunks before they are asked for.
- A separate sequential path for whole-file scans (index builds, copying out) reads 1 to 4 MB at a time and bypasses the cache so it does not evict what the UI is using.
- Do not memory-map devices or user-supplied images: a removed disk or truncated file turns a read into a crash. Memory-map only files the app wrote itself, such as a saved index.

## Parsing

- Parse in place. A B-tree node is a byte slice; fields are read through typed views at known offsets. No owned copy of a node or record on a hot path.
- Cache decoded facts that are expensive and reused (the extent list of the catalog file, B-tree header fields), not whole structures.
- Names are converted from UTF-16 once, into a reused buffer, and stored as UTF-8.

## Directory listing

- HFS+: one tree descent to the folder's first key, then a walk along the leaf chain. Entries come out in name order, so the default sort is free. Deliver in pages of a few hundred entries.
- Other sorts (date, size, kind) sort an index array on a background thread while the name-ordered rows are already on screen.
- A page that has been listed is kept per folder for instant back and forward.

## Search

The model is the one used by the fastest desktop search tools: keep every name in memory and scan them all on every keystroke. With the right layout, a full scan is faster than any index lookup is to maintain.

**Layout (struct of arrays, one index per volume):**

- `names`: every name in one contiguous UTF-8 byte buffer, separated by a zero byte.
- `folded`: the same buffer, case-folded, searched instead of `names`.
- `offsets`: start offset of each name (`u32`), ascending, so a hit position maps to an entry by binary search.
- `parent`: index of the parent entry (`u32`), so a path is rebuilt only for rows that are shown.
- `size`, `modified`, `flags` (folder, link, has resource fork, compressed), `node`: one array each.

**Build.** Use `FileSystem::scan`. For HFS+ that is a sequential read of the catalog file, parsing every leaf node in physical order without following tree links: no seeks, one pass. Parent links are resolved after the pass through a temporary map from node ID to entry index.

**Query.**

1. Fold the query. Split on spaces into terms; all must match.
2. Split `folded` into chunks at entry boundaries and scan them in parallel with a SIMD substring searcher for the rarest-looking term (the longest). On a hit, map to the entry, test the remaining terms and the filters against that entry only, then skip to the next entry.
3. Filters (kind, size, date, scope) are array comparisons on the same entry index.
4. Rank cheaply: exact name, then prefix, then word start, then substring; shorter names first. When there are many hits, select only the top few thousand for ordering and keep the rest as an unordered tail that is ordered when scrolled to.
5. Each query has a generation number. Workers check it per chunk and stop when a newer keystroke has arrived.
6. If the new query only extends the previous one, search the previous result set instead of the whole volume.

Substring matching is the default because it is predictable. Fuzzy matching can be added later as an option; it must not slow the default path.

**Persistence.** Save the arrays as a flat, versioned file per volume, keyed by the volume's identity and a change marker (HFS+: write count and modification date from the volume header; APFS: transaction ID). On open, a matching marker means the saved index is mapped and used immediately; otherwise rebuild in the background while browsing works.

## App

- Virtualized lists only. A row is built from shared, immutable data for the visible range.
- One notify per frame at most. Batches from background tasks are merged before the view is told.
- No engine call, file read or sort on the UI thread. No lock held across a paint that a background thread also takes for long.
- Thumbnails and previews decode on a background pool, cancel when the selection moves, and live in a size-bounded cache.
- The disk strip is computed once per volume from the allocation bitmap, downsampled to a few hundred buckets, and cached.

## Measured

2026-11-12, Windows 11, 24 logical cores, release builds (roadmap review items 8 and 9).

- **One-letter query at 5 million names, 8 threads** (`search_bench_5m` with `RAYON_NUM_THREADS=8`, query `a`, 3,847,598 hits, median of 20): 51.89 ms before, 42.3 ms after (42.34 and 42.06 ms in two runs) with per-piece heads merged instead of one serial select over all hits. On all 24 threads: 31.3 ms. Rare and common queries at 5M are unchanged (3 to 8 ms).
- **Results on the UI thread at 5 million hits** (`perf_entries_in_order_5m`, `--profile profiling`, best of 5): before, `show_results` ran `Hits::entries_in_order` there, 4.23 ms. After, it orders only the sorted head (4,096 rows), 0.01 ms; the whole order and its inverse are made by the background sort task, 9.7 ms (of which `entries_in_order` 4.6 ms). The cursor lookup after a sort: under 0.1 µs through the inverse, against 1.13 ms for the scan it replaces (cursor on the last row).
- **Type-to-select over 100,000 folder rows** (`perf_prefix_100k`, `--profile profiling`, prefix that matches nothing): by name ascending (binary search) 0.02 to 0.08 ms; by size (scan of the names, then of the order) 0.58 to 0.79 ms over three runs, 0.43 to 0.48 ms when the inverse of the order replaces the second scan. Cursor lookup after a sort: under 0.1 µs with the inverse, 0.021 ms by scan.

### Motion frame traces (R4, 2026-11-14)

Windows 11, 24 logical cores, 60 Hz display, `--profile profiling`, window 1182 × 762 physical pixels, list showing a host folder of 100,000 empty files (the sidebar folds were traced with that folder listed and a 3-partition GPT image open in the sidebar). How: `DISK_ATLAS_FRAME_TRACE=1` makes the app log one `disk_atlas::frame` line per frame: `cpu_ms` is the root view's render start to the end of the last paint (render, layout, prepaint, paint on the UI thread; the GPU's work and the present are not in it), `interval_ms` the time since the previous frame started, `labels` the animations that asked for the frame. Input was driven with SendInput (keys every 300 ms, a held key every 33 ms, a pointer sweep over 24 rows at 25 ms a step). "Interval max" counts only frames that follow a frame of the same animation; a dropped frame would show as about 33 ms.

| Animation | Frames | `cpu_ms` max (median) | `interval_ms` max (median) |
|---|---|---|---|
| Hover fade (one row at a time) | 124 | 3.75 (2.04) | 20.35 (16.63) |
| Selection plate glide, 10 one-step moves | 64 | 2.81 (2.12) | 17.05 (16.51) |
| Held Down key, 20 repeats at 33 ms | 3 glide frames (the first press only), 56 frames in all | 4.39 over all frames | — |
| Folder navigation slide and fade, 3 in and 2 out of the 100,000-entry folder | 50 | 4.86 (2.38) | 18.07 (16.57) |
| Details pane width, 4 toggles | 38 | 3.93 (2.17) | 18.38 (16.51) |
| Sidebar disk fold and "other partitions" fold, 5 toggles | 39 | 5.15 (2.59) | 17.99 (16.70) |
| Storage bar grow-in and ease (R15; image volume, not the 100,000 folder) | 15 | 4.23 (3.25) | 17.01 (16.73) |

Reduced motion (`DISK_ATLAS_REDUCED_MOTION=1`), same folder: navigation is a 90 ms fade (6 frames each, `cpu_ms` max 4.76), the details pane fades over 90 ms in and 63 ms out (`cpu_ms` max 2.99), the selection moves with no plate (no glide frames), hover `cpu_ms` max 8.66.

No traced frame came near 16.7 ms of UI-thread work: the largest `cpu_ms` of the whole full-motion run was 11.33 ms, the first frame after the window opened. One interval over 25 ms was seen, on a hover frame in the 114-entry Home folder (26.90 ms after `cpu_ms` 1.18 ms, followed by a 7.17 ms interval): a late present, not render cost. The selection-plate spike passed, so the glide stays.

### Host sizes and the watcher (R14.1, R14.2, 2026-11-14)

Windows 11, NTFS on NVMe, 24 logical cores, release build, `cargo test --release -p disk-atlas-lib --lib -- --ignored --nocapture --test-threads=1`.

| What | Measured | Budget | |
|---|---|---|---|
| `host_sizes_heap_1m_folders`: `heap_bytes`, 1,000,000 folders | 82.7 MiB (86.7 bytes per folder) | 96 MiB | met |
| `host_sizes_walk_500k_files`: `request(root, Visible)` to `Exact`, 500,000 files in 5,050 folders, warm cache, 4 workers | 213 ms (median of 3) | 3 s | met |
| `one_flush_of_1000_folders_into_invalidate`: 1,000 `HostSizes::invalidate` calls in a row, 11 files per folder, median of 3 | 95.1 ms (95.1 µs per folder); the 2026-11-14 single sample was 140 ms | 200 ms (restated from 50 ms) | met |
| Baseline for the row above: bare `read_dir` + `DirEntry::metadata` over the same 1,000 folders, printed by the test itself, median of 3 | 69.0 ms (earlier scratch program: 70 to 104 ms) | — | — |

The miss against 50 ms was the platform, not the watcher: reading 1,000 folders one after another costs more than 50 ms on NTFS before Disk Atlas does anything. Fable restated the budget on 2026-11-14 (`r10-everyday.md` §3.3 and §14): 200 µs or less per changed folder, 200 ms for 1,000, on every platform. It keeps the cost of applying a flush well inside `WATCH_FLUSH` (500 ms); the engine is unchanged, and batches are merged and capped on the app side in R14.3. The test now prints the 200 ms budget, takes the median of 3 and prints the bare-loop baseline beside it (done with R14.3, 2026-11-14). Still to do: the same test on Linux and macOS.

## Build settings to adopt in M0

- Release: `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`. Keep unwinding so a parser bug in one volume cannot take the app down.
- Development: optimize dependencies (`[profile.dev.package."*"] opt-level = 3`) so GPUI is usable in debug builds.

## Proving it

- `criterion` benchmarks in `crates/lib/benches` for: B-tree leaf iteration, directory listing, index build, query over a synthetic million-name index, UDIF chunk reads.
- `tracing` spans around every user-visible operation, with a debug overlay or log line showing the last operation's time.
- A change that claims a speedup reports before and after numbers from the same machine.
