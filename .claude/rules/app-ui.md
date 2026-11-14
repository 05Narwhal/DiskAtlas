---
paths:
  - "crates/app/**"
---

# App rules (crates/app)

- Check the GPUI API against the pinned version's source or examples in `~/.cargo/registry/src/*/gpui-*` before writing a call. Do not rely on memory.
- The UI thread never touches a disk. Engine calls run on the background executor; results arrive in batches and trigger at most one notify per frame.
- Every list that can exceed a screen is virtualized. A row renders from borrowed or shared data; no per-row allocation or clone of the entry list during paint.
- Every async result carries the request generation it belongs to. A result for a folder or query the user has already left is dropped.
- Colors, spacing, radii and type sizes come from `theme`. No color or size literals in components.
- Motion comes from `theme::motion`; no literal durations. Every animation has a reduced-motion form.
- The accent color means one thing: what the user has selected, and where it is on the disk. Do not use it for decoration, hover, or buttons.
- Every action has a keyboard path. Focus is always visible.
- States are designed, not defaulted: loading, empty, error, unsupported filesystem, encrypted volume, needs elevation. Copy says what happened and what to do next.
- Design tokens, layout and components: `.claude/docs/ui-design.md`.
