# Orchestration

How the main session splits work across `opus-engineer`, `sonnet-builder` and `haiku-runner`. The aim is that every task is done by the cheapest model that will do it right the first time, and that the expensive thinking happens once, in one place.

## Cost policy (Julian, 2026-11-15, overrides everything below)

Opus subagents burned the usage limit within minutes. So:

- **No subagent runs on Opus.** `opus-engineer` is pinned to Sonnet. Never pass `model: opus` to the Agent tool. If a task truly needs Opus, stop and ask Julian for that one task first.
- **No second-Opus review.** The review gate for `crates/lib` is: the lead reads the diff, runs fmt, clippy and tests, and the fuzz target or an external oracle (fsck, mkfs, real tools in WSL) where one exists. A review subagent, if used at all, is Sonnet and reads only the diff.
- **One subagent at a time**, and only when the task is big enough that its brief is shorter than the work. Tiny or tightly coupled work is done inline by the lead.
- **Every brief carries a budget**: files it owns, the sections to read (not whole docs), and "stop and report after N attempts at a failing test". No exploring, no re-reading, no whole-file dumps.
- **The lead does the thinking** (the session model Julian picked). Subagents build from a finished design.
- Run `graphify query` before reading source, as the hooks demand.

## Lean build policy (Julian, 2026-11-15, overrides the designs' test lists and the gates below)

The plan was spending more time on proof than on product. From now on:

**Tests, per task**
- **Engine parsers of on-disk bytes** (rule 2 is not traded): one test with valid input (round trip or a fixture) and one with malformed input (truncated or out-of-range). Add a fuzz target and seeds as before, but do not run it as a gate on the task: fuzzing runs in one batch per lane checkpoint in WSL (see "Batched checks").
- **Other engine code** (sizes, watcher, extract, search): two or three tests of the core behaviour and the one edge case most likely to be wrong. No `#[ignore]` timing test unless the task's design names a budget that is in doubt; then one timing, printed once.
- **App code**: test only pure logic that has real branches (layout maths, sorting, parsers of settings, state machines). No `TestAppContext` tests of rendering, focus or menus, and no table test that lists every label or row. A menu or view is checked by looking at it.
- **The design files' "Tests:" lists are a menu, not a checklist.** Pick the cases that cover hostile input and the hardest branch; skip the rest without noting it.

**Checks, per task**: `cargo check -p <crate>` and the focused tests (`cargo test -p <crate> <filter>`). Clippy, fmt and `cargo test --workspace` run once at a lane checkpoint, not after every task.

**UI gate**: one screenshot of the running app in the dark theme per task with something to see. Both themes once per lane checkpoint, on the screens that changed.

**Batched checks, at each lane checkpoint** (about every two milestones): `cargo fmt --all`, clippy `-D warnings`, `cargo test --workspace`, the light-theme screenshots, and, when the lane added a parser, its fuzz targets for 60 s each in WSL. A fuzz crash is fixed in the next task.

**Process**
- **Do it inline first.** A lane session builds its own tasks. A subagent is used only for a task that is large, self-contained, and whose brief is much shorter than the work (for example a whole format reader); one at a time, as the cost policy says. Never one subagent per subtask.
- **"Verify first" items** stay only for GPUI calls (rule 6) and for on-disk offsets in a parser. Everything else is trusted from the design.
- **Records are short.** A roadmap line gets one sentence of what was built and one of what is left. "Changes during build" holds only what changes how later work is done.
- **Julian looks at the app once per lane checkpoint**, not once per milestone; the lane keeps working while it waits, unless what he is asked about blocks the next task.
- **Parallel lanes** work in `.worktrees/lane-*` on their own branches, own disjoint areas (see the roadmap's "Lanes"), commit freely, never push, and never merge into `dev` themselves: at a checkpoint they rebase onto `dev` and report "ready to merge"; the lead session merges and runs the batched checks.

**Since 2026-11-12 the main session is Opus or Sonnet, not Fable.** The expensive thinking has happened: it is in `.claude/docs/design/`. Read "Fable" in this file as "the lead", with three differences:

- **The lead does not design.** Where this file says Fable decides, the design file has already decided. A gap the design leaves is the lead's to fill when it is small (a name, a place for a helper, an error message) and is recorded under "Changes during build". A gap that changes a public interface, a data layout, a threading rule or a security bound is a stop-and-ask (`design/README.md` rule 4).
- **(Superseded by the cost policy above: no Opus subagent reviews.) Review of engine changes is by Opus.** When the lead is Opus, it reviews as the gate below says. When the lead is Sonnet, a change to `crates/lib` made by `opus-engineer` is reviewed by a second `opus-engineer` run whose brief is the diff, the design section and the review gate; the lead still runs the checks itself and reads the report.
- **Root-cause debugging** stays with the lead up to two failed attempts at a fix, then goes up a tier like any other task; a bug that survives Opus is written up for Fable with the evidence.

## What the main session keeps

Fable does not delegate thinking. It keeps:

- Design: traits, public API, data layouts, cache and threading policy, the search index design, UI state architecture.
- Interpreting on-disk formats where the spec is ambiguous or real disks disagree with it.
- Root-cause debugging. A subagent may gather evidence; the diagnosis is made here.
- Review of every change to `crates/lib`, and of anything on a hot path in `crates/app`.
- Integration: wiring finished pieces together, resolving conflicts between them, the final check before reporting to Julian.

Fable does not do mechanical work itself. If a task needs no decisions, it goes to a subagent even when it would be quick to do inline.

## Routing test

Ask these in order and stop at the first yes.

1. Is a decision still open (what the API is, how it should behave, which trade-off to take)? Fable decides first. Then route the implementation.
2. Would a subtle mistake silently corrupt results, crash on a damaged disk, or miss a performance budget? `opus-engineer`.
3. Is it ordinary feature work with a clear spec and an obvious way to check it? `sonnet-builder`.
4. Could it be written as an exact list of edits or commands? `haiku-runner`.

| Task | Goes to | Why |
|---|---|---|
| Shape of the `FileSystem` trait | Fable | Design |
| HFS+ B-tree node reader and iterator | opus-engineer | Subtle, hostile input, hot path |
| Extent resolution with the overflow file | opus-engineer | Subtle |
| decmpfs decompression (zlib, LZVN, LZFSE) | opus-engineer | Subtle, performance |
| Journal replay overlay | Fable designs, opus-engineer builds | Correctness of everything depends on it |
| Search index build and query | Fable designs, opus-engineer builds | Performance architecture |
| Windows, Linux, macOS raw device access | opus-engineer | Platform FFI, alignment rules |
| Virtualized file list with streaming load | opus-engineer | Hot path, async correctness |
| GPT and MBR parsing | sonnet-builder | Simple, well documented |
| HFS+ volume header parsing | sonnet-builder | Fixed layout |
| Sidebar, breadcrumb, top bar, details pane | sonnet-builder | From the design spec |
| Theme tokens module | sonnet-builder | From the design spec |
| Native filesystem provider over `std::fs` | sonnet-builder | Plumbing |
| Byte fixtures and unit tests for a finished parser | sonnet-builder | Clear spec |
| Rename `cargo.toml` to `Cargo.toml` | haiku-runner | Mechanical |
| Add GPL headers, create module stubs | haiku-runner | Mechanical |
| Run clippy and apply the listed fixes | haiku-runner | Mechanical |
| "Where is X defined?" | haiku-runner | Lookup |

When unsure between two tiers, pick the higher one for `crates/lib` and the lower one for everything else.

## Writing a brief

A subagent starts with no memory of the conversation. The brief is everything it knows. Use this shape:

```
Goal: one or two sentences, including what this is for.
You own: the exact files to create or edit. Nothing else may change.
Interfaces: the signatures, types or design tokens to match, pasted in or named by path and line.
Read first: the doc sections and source files that matter (name sections, not whole docs).
Behaviour: what it must do, including the edge cases already known.
Tests: what to add and what must pass.
Done means: the commands that must succeed.
Out of scope: the nearby things not to touch.
Report: what to send back.
```

Paste interface code into the brief rather than describing it. Give byte offsets and field names for parsers. For UI, name the component and the tokens.

## Running in parallel

- Up to three subagents at once, one per tier at most unless the tasks are plainly independent.
- Parallel tasks own disjoint files. Before a parallel batch, have `haiku-runner` create the module stubs and `mod` lines so nobody edits a shared `mod.rs` or `lib.rs`.
- Shared files (`Cargo.toml`, `lib.rs`, the theme, the `FileSystem` trait) are edited by the main session or by one agent at a time.
- Cargo holds a lock on the build directory, so concurrent `cargo` runs queue. Tell subagents to check their own crate (`-p`), and run the workspace-wide check once from the main session after the batch.

## Review gate

Nothing counts as done on a subagent's word. (The lean build policy above sets how much testing and checking each step needs.)

1. Read the diff. For `crates/lib`, check every disk-derived value is validated, every loop is bounded, and the tests include malformed input.
2. Run `cargo check -p <crate>` and the focused tests; clippy and the whole suite at the lane checkpoint.
3. If it is wrong: small fix, do it inline; wrong approach, send it back to the same agent with what was wrong; too hard for the tier, re-route upward with the failed attempt as context.
4. UI changes are not verified by tests. The lead runs the app, takes a dark-theme screenshot with the tool in `tools/screenshot/`, and looks at it before accepting; both themes at the checkpoint. Julian tries the app at each lane checkpoint with `cargo run -p disk-atlas`; say exactly what to look at.

## Escalation and cost

- A subagent that reports "this needs a decision" has done its job. Decide, then re-brief.
- A task that fails twice at one tier moves up a tier. A task that turned out trivial is a note to route that kind lower next time.
- Do not spawn an agent to read a file the main session needs to understand anyway.
- Do not spawn several agents to "explore" in parallel. One lookup, one agent.

## Milestones

Work through your lane in `.claude/docs/roadmap.md` ("Lanes"), one milestone after another without waiting for a go-ahead between them. At a checkpoint (about every two milestones): rebase onto `dev`, run the batched checks, update the roadmap lines, and send Julian a short summary of what works, what was cut and what to try. Keep going on the next milestone unless his answer is needed for it.
