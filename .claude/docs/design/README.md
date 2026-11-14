# Milestone designs, M3 leftovers to M8

Written by the main session (Fable) on 2026-11-11, at the end of M3, so the remaining milestones can be run by a lower model as lead. Each file holds the decisions, the interfaces as Rust, the algorithms with their bounds, and the work cut into briefs that name an owner tier and the files it owns.

| File | Covers |
|---|---|
| `m3-leftovers.md` | The conformance pass (what to change for each possible answer), Linux and macOS device access, small debts from M3 |
| `m4-search.md` | `scan`, the index layout, the query engine, the saved index, search mode in the app |
| `m5-quicklook-copy.md` | `open` and extended attributes in the trait, preview providers, the bytes view, copying out with AppleDouble |
| `m6-hfs.md` | Classic HFS on the shared B-tree reader, Mac script encodings, DiskCopy 4.2 |
| `m7-apfs.md` | Containers, checkpoints, object maps, the file-system tree, snapshots, sealed volumes, password unlock |
| `m8-formats-plugins.md` | FAT, exFAT, NTFS, ext4, ISO 9660, UDF; the provider protocol for plugins and the elevated helper; settings, icon view, packaging |
| `r1-redesign.md` | **Approved 2026-11-13, decisions 17–28 included.** Visual identity, motion, sidebar and disks, host file operations, Quick Look as a window, preview coverage, the GPU 3D viewer; milestones R1–R9, Q1–Q3, G0–G1 and decisions 17–28 |
| `r10-everyday.md` | **Approved 2026-11-13, decisions 29–42 included. Start here for the R milestones.** Right-click menus, Properties, Settings (store and window), folder sizes on every volume and on the host with a watcher, the storage bar that replaces the scale strip, Mac housekeeping files hidden and put to use; milestones R10–R18, decisions 29–42, and **one build order for R1's milestones and these together (its §10)**. Reviewed by Fable against the code the same day (its §13). Mockups: https://claude.ai/artifact/St9vwbp8Ber5eQuB79CekT |
| `provider-protocol.md` | The byte-level protocol between the app and a separate process (extensions, the elevated helper): framing, every message, limits, how each peer is started |

## What Fable has already done, and what still needs it

On 2026-11-12 Fable went through every design and did the work the designs had reserved for it, so that **Opus or Sonnet can lead every remaining milestone**. Done, and marked "Done" in each file's task table:

- M5: the trait additions (`open`, `xattrs`, `read_xattr`, `finder_info`, `local_times`) and the preview pipeline (`crates/app/src/preview/mod.rs`), both with tests.
- M6: the layout tables checked against a second source; nothing changed.
- M7: the layout tables checked and five additions made (the sealed-volume one would have broken every modern system disk); the mount API for containers, locked volumes and snapshots built; the encryption brief written (§9.5).
- M8: the crate check; the provider protocol specified byte by byte, including how the elevated helper is started.
- A review of what M0–M4 built: `roadmap.md`, "Review, 2026-11-12".

What is left that is not ordinary execution. None of it needs Fable by default; each line says who it needs:

| Item | Needs |
|---|---|
| The conformance pass (M3) and the timing tasks at the end of M5 and M7 | Julian's real disks, then the lead |
| The Julian decisions below (now 16) | Julian |
| The elevated helper's four checks with a real prompt (`provider-protocol.md` §8.2) | Julian at the keyboard, then the lead |
| A stop-and-ask case (rule 4 below) | Fable, or Julian if he would rather decide |

## How to use these

1. **The lead model's job changes from design to execution.** For each task: copy the brief, paste in the interface code from the design file, add the current line numbers of the files named, send it to the tier named. Review with the gate in `orchestration.md`. Do not redesign; where the design is wrong, see rule 4.
2. **Order inside a milestone** is the order of the task list at the end of each file. Tasks marked "parallel" own disjoint files and may run together after the stubs task.
3. **Every design lists "Verify first" items.** These are facts the design depends on that were written from memory of a specification or of the GPUI source and must be confirmed against the primary source before code is written (rule 6 for GPUI, `filesystems.md` for formats). The brief for that task must include the verify step. If the source disagrees with the design on a number or a field offset, the source wins and the design file is corrected in the same change. That is not a redesign.
4. **When to stop and ask for Fable (or Julian).** A disagreement that changes a public interface, a data layout, a threading rule or a security bound; a real disk that contradicts the design; a benchmark that misses its budget by more than 2x after the obvious fixes. Write down what was found and stop. Everything else, the lead decides and notes in the design file under "Changes during build".
5. The rules in `.claude/CLAUDE.md` are unchanged and outrank these files: read-only, hostile bytes, engine free of GPUI, nothing blocking the UI thread, measured speed, GPUI from source, no dependency without approval.

## Decisions that are Julian's

These are calls the designs make that only Julian can confirm. Each design works as written if he says yes to all of them; the affected section says what changes otherwise.

Decisions 17 to 28 are in `r1-redesign.md` §10 and 29 to 42 in `r10-everyday.md` §11. **Julian approved all of 17 to 42 on 2026-11-13.** That covers the new crates `trash`, `notify` and `wgpu`, the preview crates of Q1 and Q2, and the `unsafe` calls those files list, each still written by O with a `// SAFETY:` comment. It does not cover the Q3 crates (asked for one at a time), and it does not remove two gates: the G0 spike before G1, and the four prompt checks with Julian at the keyboard before R9. The sixteen below keep their own status; where an R milestone stands on one of them (9 for the checksum, 13 and 14 for the settings file and the cache folder), the lead asks Julian before that task and not before.

| # | Decision | Design's choice | Where |
|---|---|---|---|
| 1 | Dependencies for search | `rayon` and `memchr` in the engine (both already in `Cargo.lock` through GPUI) | m4 |
| 2 | Saved index without memory mapping | Read into memory, no `memmap2`, no `unsafe`; revisit only if the 200 ms budget is missed | m4 |
| 3 | Searching "This computer" | Index built on first Ctrl+F, kept for the session, never saved | m4 |
| 4 | Copy conflicts | Never overwrite, never ask: a taken name gets " (2)" | m5 |
| 5 | Resource forks on copy | Off by default; one checkbox, "Keep resource forks and Finder information", remembered | m5 |
| 6 | Symbolic links on copy | Not copied; counted and reported | m5 |
| 7 | Classic HFS name lookup without Apple's ordering table | Lookup scans the folder; no table copied from APSL or GPL sources | m6 |
| 8 | Multi-byte Mac scripts (Japanese, Chinese, Korean) | Deferred; shown through Mac OS Roman | m6 |
| 9 | Dependencies for APFS unlock | `aes`, `sha2`, `hmac`, `pbkdf2` (RustCrypto, all already in `Cargo.lock`); XTS and key unwrap written in-house on top of `aes` | m7 |
| 10 | APFS snapshots in the sidebar | A collapsed "N snapshots" row under the volume | m7 |
| 11 | Crates or in-house for M8 formats | In-house for FAT, exFAT, ISO 9660, UDF; the `ext4-view` crate behind a panic guard for ext (checked 2026-11-12: passes). NTFS: see 15 | m8 |
| 12 | Plugin mechanism | Separate processes speaking a framed protocol over pipes, with the host serving the block reads. No dynamic libraries, no WebAssembly runtime | m8 |
| 13 | Settings file | Hand-written `key = value` file, no new dependency | m8 |
| 14 | What rule 1 allows the app to write | Three things, all on the host and none on a device or image: files the user copies out; the app's own cache folder (saved search indexes, since M4); the app's own config folder (settings, M8). Reword rule 1 in `.claude/CLAUDE.md` to say exactly that. If Julian says no to the cache, saving indexes is removed and every index is rebuilt each session | roadmap review, item 2 |
| 15 | NTFS in M8 | Wait and write it in-house later, because the `ntfs` crate has had no release since June 2023; the alternative is the crate on probation behind a fuzz gate | m8 §1 |
| 16 | Whether `.claude/` (the plan, the designs, the agent definitions) is kept in git | Yes: today it is ignored, so none of it is versioned or backed up with the code | roadmap review, item 1 |

## Conventions used in every design

- "O", "S", "H" are `opus-engineer`, `sonnet-builder`, `haiku-runner`. "Lead" is the main session, whatever model runs it.
- Measured numbers follow the M3 pattern: an `#[ignore]` test run with `--release -- --ignored --nocapture` that prints the figure, named in the roadmap line. No `criterion` unless Julian asks for it.
- Every new raw-byte parser gets a fuzz target and seeds (root `CLAUDE.md`, "Commands").
- Bounds are stated as constants with names. A brief must carry them over verbatim.
- New `Metadata`, `VolumeInfo` and `DirEntry` fields are additive; the structs are `#[non_exhaustive]` for this.
