---
paths:
  - "crates/lib/**"
---

# Engine rules (crates/lib)

- Read-only. No code path opens a device or image with write access.
- Untrusted input. Validate every on-disk length, offset, count and index before use. Checked arithmetic on disk values. Bounded loops on linked structures. Errors, never panics. Fuzz targets exist for every parser that takes raw bytes.
- No `gpui`, no async runtime, no UI types. Public API is blocking, `Send + Sync`, `&self`.
- Parse in place from the block buffer through typed big-endian or little-endian views. Do not build owned copies of on-disk structures on hot paths.
- Errors are `thiserror` enums that say what was being read and where (structure, byte offset or block number). `anyhow` belongs to the app.
- Byte order: HFS and HFS+ are big-endian; the decmpfs header inside them is little-endian; APFS, GPT and MBR are little-endian; APM is big-endian.
- A new filesystem implements the `FileSystem` trait and registers a probe. It does not get special cases in the app.
- Format details and traps: `.claude/docs/filesystems.md`. Budgets: `.claude/docs/performance.md`.
