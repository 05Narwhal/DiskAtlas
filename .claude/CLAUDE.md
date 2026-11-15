# Disk Atlas

A cross-platform file explorer in Rust and GPUI that reads disks the host operating system cannot: HFS, HFS+ and HFSX first, APFS after, then the common non-Apple formats. The product promise is speed: instant directory listings, instant search, no webview.

Two crates:

- `crates/lib` (`disk-atlas-lib`): the engine. Block devices, disk images, partition maps, filesystems, search index. No UI dependency, usable on its own.
- `crates/app` (`disk-atlas`): the GPUI desktop application.

Current state (2026-11-12): M0 to M4 are built (the explorer on the host filesystem, disks, images and partition maps, HFS+ and HFSX, search). M5 to M8 are designed in `.claude/docs/design/` and not built, apart from the interfaces Fable wrote ahead (the M5 trait additions and preview pipeline, the M7 mount API). The plan lives in `.claude/docs/roadmap.md`; its "Review, 2026-11-12" section lists what to do before M5.

Since 2026-11-13 the next work is the R milestones, approved by Julian and ahead of M6: the redesign (`design/r1-redesign.md`) and the everyday features (`design/r10-everyday.md`: menus, Properties, Settings, folder sizes, the storage bar, Mac housekeeping files). One order for both is in the roadmap's "R" section. Sonnet can lead them; the engine tasks in R12, R13, R14 and R17 are `opus-engineer` work with the Opus review of `orchestration.md`.

## Commands

```
cargo check --workspace
cargo test --workspace                 # focused: cargo test -p disk-atlas-lib <filter>
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
cargo run -p disk-atlas

# Fuzzing (Linux or macOS, nightly, cargo install cargo-fuzz). Seeds first:
DISK_ATLAS_FUZZ_SEEDS=$PWD/fuzz/seeds cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored
cargo +nightly fuzz run --fuzz-dir fuzz hfsplus_mount fuzz/corpus/hfsplus_mount fuzz/seeds/hfsplus_mount -- -max_len=1048576
```

A new raw-byte parser gets a target in `fuzz/fuzz_targets/` and, when it has a valid form, seeds in `crates/lib/src/fs/hfsplus/fuzz_seeds.rs` (or a sibling for its format).

## Rules that are never traded away

1. **Write wherever a format supports it; read-only where it does not yet.** Disk Atlas reads and writes everywhere it can (Julian, 2026-11-15). Each filesystem and image format gets as much functionality as possible, writing included, as its writer is built and verified. A format with no writer yet stays read-only, and no code path opens a device or image for writing until that format's writer exists and has passed its checks. Writes to a disk or image need an explicit user action and a confirmation in the app; never silent, never automatic. A host operation never targets a device or an image the app has open. The app also writes to its own config folder (the settings file, decision 13) and cache folder (saved search indexes, decision 14). Copy, move, rename, new folder, Recycle Bin and undo on the host filesystem are as in `r1-redesign.md` §4.
2. **On-disk bytes are hostile.** Every length, offset, count and index read from a disk is validated before use. Parsers return errors; they never panic, never index unchecked, never do unchecked arithmetic on disk values, and never loop without a bound. `unsafe` needs a `// SAFETY:` comment and the main session's approval.
3. **The engine never depends on GPUI.** `crates/lib` exposes a blocking, thread-safe API. Async, tasks and UI state live in `crates/app`.
4. **Nothing blocks the UI thread.** All disk and filesystem work runs on background executors and reaches the UI in batches.
5. **Fast is a measurement.** A performance claim needs a benchmark or a traced timing. Budgets are in `.claude/docs/performance.md`.
6. **GPUI from source, not from memory.** The GPUI API changes between releases. Before writing GPUI code, read the pinned version's source under `~/.cargo/registry/src/*/gpui-*` or its examples. Do not guess signatures.
7. **No new dependency without the main session's approval.** It must be compatible with GPL-3.0-or-later, maintained, and worth its compile time.
8. Every Rust source file starts with the GPL header used in `crates/lib/src/lib.rs`.
9. Do not commit or push unless Julian asks. Work happens on `dev`.

## Who does what

This repository runs a team of models. This replaces the global default of a Sonnet main session with `Explore` and `quick-fix` helpers.

| Model | Role | Gets |
|---|---|---|
| Main session (Opus or Sonnet) | Lead | Runs the roadmap from the designs: writes the briefs, reviews every change, integrates, reports to Julian. Decides small things and records them under "Changes during build" in the design file |
| Fable | Architect, on call | Wrote the designs and the interfaces they stand on. Needed again only for the stop-and-ask cases in `.claude/docs/design/README.md` rule 4: a design that turns out wrong, a real disk that contradicts it, a budget missed by more than 2x |
| `opus-engineer` (runs on Sonnet) | Careful implementer | Hard, well-specified implementation: B-trees, extent and fork readers, decompression, journal overlay, the search index, concurrency, platform device access |
| `sonnet-builder` | Implementer | Standard feature work: GPUI components from the design spec, simple parsers (GPT, MBR), plumbing, tests, fixtures |
| `haiku-runner` | Runner | Mechanical work: renames, headers, stubs, formatting and lint fixes, running checks and reporting, file lookups |

If you are the main session: follow `.claude/docs/orchestration.md` before delegating anything, and work from the design files. Do not redesign. When you reach a stop-and-ask case, stop, write down what you found in the design file, and tell Julian it needs Fable. Opus is the better lead for M6 and M7 (format work, where the review of engine changes carries the weight); Sonnet leads M5's interface tasks and M8's interface and packaging comfortably.

If you are a subagent: your agent file is your job description. Do the brief you were given, nothing wider.

**Cost policy (Julian, 2026-11-15):** no subagent runs on Opus, no Opus review passes, one subagent at a time, every brief has a scope and attempt budget. Details: `.claude/docs/orchestration.md` "Cost policy". Where a document says Opus does a task, read it as: the lead session does it, or `opus-engineer` (Sonnet) builds it from the design and the lead reviews.

## Read these when the work touches them

- `.claude/docs/orchestration.md`: routing rules, the brief template, review gates.
- `.claude/docs/architecture.md`: layers, module map, core traits, threading and error model.
- `.claude/docs/filesystems.md`: on-disk format notes and traps for partition maps, disk images, HFS, HFS+, HFSX, APFS.
- `.claude/docs/performance.md`: budgets and the techniques that meet them, including the search index design.
- `.claude/docs/ui-design.md`: design tokens, layout, components, keyboard map, states.
- `.claude/docs/roadmap.md`: milestones and tasks, each tagged with the model that should do it.
- `.claude/docs/design/`: the designs for M4 to M8 and the M3 leftovers, written in advance by Fable so a lower model can lead. Start at its `README.md`; it says when to stop and ask.
