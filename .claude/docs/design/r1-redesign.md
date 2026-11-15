# R1. Design pass and polish pass

Written by the lead (Opus) on 2026-11-13, from Julian's brief of the same day. This is the "a design turned out wrong" case of `README.md` rule 4, with Julian's permission to reopen `ui-design.md`. **Status: approved by Julian on 2026-11-13, decisions 17 to 28 (§10) included.** The spike before G1 and the four prompt checks before R9 stay as gates. §1 to §2 replace the matching sections of `ui-design.md` (the first task of R2), and the milestones in §9 are in `roadmap.md` ahead of M6, **in the order of `r10-everyday.md` §10**, which adds R10 to R18 and changes seven things here (its §1).

Mockups (six boards, each with a Day/Night switch in Tweaks): https://claude.ai/artifact/XPBNo2GLQD3PpoQGLkjJnz
## 0. What was seen

The app was run on 2026-11-13 (debug build, Windows 11, not elevated) and screenshotted:

- The window is gray on gray: `bg #1F1F1E` against `surface #171716` differ by 1.1:1, so the sheet does not read as a surface. The details pane for a folder holds two lines and 700 px of nothing.
- Sidebar disk names are cut ("Lexar SSD NM790 ·"), and the Windows system disk is listed under Disks although Disk Atlas adds nothing to it.
- Clicking a disk that needs administrator rights highlights the row and does nothing else. `sources.rs:425` maps `AccessDenied` to `Availability::NeedsElevation`, and the state exists (`explorer.rs:578`), but nothing tells the user at startup or on the sidebar.
- Quick Look: Space after clicking a tab does not close the panel. Cause in §5.1.

How this happened: several milestones were accepted from tests alone. From R1 on, the gate in §9 applies: no UI task is accepted without a screenshot of the running window.

## 1. Visual identity

### 1.1 Direction: a survey chart of the disk

Disk Atlas draws a disk the way a survey chart draws ground: to scale, in layers, with a legend. Four ideas come from that and nothing else is decoration:

1. **Chart paper and deep water.** By day the surfaces are a cool, slightly green paper. By night they are deep blue-green water. No neutral grays; the hue is what makes it this product and not every dark app.
2. **The overprint.** On nautical and aeronautical charts, magenta is the overprint: the navigator's own marks on top of the printed chart. Disk Atlas keeps magenta for exactly that: what the user selected and where it lives on the disk. It is never a button, a hover or a decoration. This keeps the existing rule and gives it a reason.
3. **A legend of kinds.** Every file kind has one glyph and one muted tint, like land-use classes on a map. They tint list icons, icon tiles and the space map. None is pink, so the overprint stays unique.
4. **Scale and borders.** The disk strip becomes a scale bar with real ticks. The space map draws folders as regions, with higher-level borders drawn thicker, as country borders are. Free space is drawn as water, hatched, its label in italic (the cartographic convention for water names).

What stays from `ui-design.md`: one accent with one meaning, Atkinson Hyperlegible (chosen for look-alike characters in old names), italic for aliases, tabular figures, plain-word copy, the keyboard map.

### 1.2 Color

Contrast is computed (WCAG relative luminance), against `sheet` unless noted.

| Token | Day | Night | Use |
|---|---|---|---|
| `desk` | `#C9D2CF` | `#0B1113` | Behind floating windows in mockups only |
| `chrome` | `#E4EAE8` | `#121A1C` | Window material: top bar, sidebar. Under Mica where available (§1.6) |
| `sheet` | `#FAFBFA` | `#172124` | Content panel |
| `raised` | `#FFFFFF` | `#24323A` | Popovers, menus, the selected segment of a segmented control |
| `field` | `#D9E1DE` | `#1C272A` | Search field, segmented-control track |
| `hover` | `#EBF0EE` | `#243236` | Hover, current sidebar row |
| `well` | `#EEF2F0` | `#101719` | Preview wells |
| `track` | `#E8EDEB` | `#1F2B2F` | Disk strip background, bar tracks |
| `line` | `#D5DDDA` | `#2D3C40` | Separators |
| `ink` | `#15201D` (16.1:1) | `#E4ECE9` (13.7:1) | Text, filled button |
| `ink2` | `#46534F` (7.8:1) | `#AEBBB7` (8.3:1) | Secondary text |
| `ink3` | `#5E6B67` (5.4:1; 4.6:1 on `chrome`) | `#8D9C98` (5.7:1; 4.6:1 on `hover`) | Labels, hints |
| `mark` | `#B0166C` (white on it 6.6:1) | `#C8207F` (white 5.3:1; 3.1:1 against `sheet`) | Selected row fill |
| `pin` | `#B0166C` | `#F25CA2` (5.3:1) | Overprint lines and dots: file pieces, current volume, the selected region |
| `relief` | `#2B3532` | `#C9D6D2` | Disk strip occupancy shading, drawn at 4 to 80 percent |
| `water` / `water_line` | `#DCEAEF` / `#A9C7D3` | `#183039` / `#26495A` | Free space hatching |

Kind tints (all ≥ 4.5:1 on `sheet`, so they also work as text):

| Kind | Day | Night | Kind | Day | Night |
|---|---|---|---|---|---|
| Folder | `#4E7487` | `#8EB4C6` | Archive | `#7D5F45` | `#C4A285` |
| Document | `#2F6DA8` | `#7FB0E3` | Application | `#4A55B8` | `#9AA3F0` |
| Picture | `#2C7D5F` | `#6FC3A0` | 3D model | `#127C8A` | `#5CC6D3` |
| Audio | `#7A5BB0` | `#B79BE6` | Font | `#6B6158` | `#BFB3A6` |
| Movie | `#B5582F` | `#EB9A72` | Disk image | `#556B2F` | `#A8BF7A` |
| Code and data | `#8A6D12` | `#D9B65A` | Other | `#66726F` | `#A7B2AF` |

A kind's wash (icon tile, space-map fill) is its tint at 12 % (day) or 13 % (night) alpha. Kind is never shown by color alone: the glyph or the label always goes with it.

### 1.3 Type

Atkinson Hyperlegible Next for the interface, Atkinson Hyperlegible Mono for bytes and offsets (as today). Weights 400, 500, 700. Tabular figures wherever numbers align.

| Role | Size / weight | Where |
|---|---|---|
| Display | 22 / 700, −0.01 em | Disk view title, empty-state heads |
| Title | 20 / 700 | Volume title above the list |
| Heading | 15 / 700 | Quick Look file name, details-pane name, section heads |
| Body | 13 / 400 | Rows, buttons, most text |
| Small | 12 / 400 | Secondary columns, captions, sidebar group labels (12 / 700) |
| Map label | 11 / 700, +0.06 em | Space-map region names; water names italic |
| Mono | 12 / 400 | Bytes view |

No all-caps labels, anywhere. Sidebar group labels are sentence case at 12 / 700 in `ink3`.

### 1.4 Space, density, radius

- Spacing scale: 2, 4, 8, 12, 16, 24, 32, 48. This replaces 4, 6, 8, 14, 16; `SPACE_6` and `SPACE_14` go.
- Density, a setting with three steps that changes row heights only. Compact: rows 24, sidebar rows 24, 16 px icons. **Regular (default)**: rows 30, sidebar 28, 24 px icon tiles or thumbnails. Roomy: rows 40, sidebar 32, 32 px thumbnails.
- Radius: 4 small marks, 6 rows and controls, 8 fields and buttons, 10 wells and popovers, 12 the sheet's corner and the Quick Look content area.
- Sizes: top bar 48, sidebar 240, details pane 300 (both collapsible), controls 32 (28 inside segmented controls).

### 1.5 Elevation

| Level | What | Day | Night |
|---|---|---|---|
| 0 | Window material: top bar, sidebar | `chrome`, or Mica (§1.6) | same |
| 1 | The sheet | `sheet`, `0 0 0 1px line`, `0 1px 2px rgba(20,32,29,.06)` | `sheet`, `0 0 0 1px #22303A` |
| 2 | Popovers, menus, filter menus, toasts | `raised`, `0 0 0 1px line`, `0 8px 24px rgba(20,32,29,.14)` | `raised`, `0 0 0 1px line`, `0 8px 24px rgba(0,0,0,.35)` |
| 3 | Quick Look and dialogs | separate OS windows with the system's shadow | same |

GPUI draws these with `box_shadow` (no new code).

### 1.6 Window material: what GPUI 0.2.2 allows

Read from the source on 2026-11-13:

- `WindowBackgroundAppearance` is `Opaque`, `Transparent` or `Blurred` (`platform.rs:1310`). On Windows, `Blurred` is acrylic through the undocumented `SetWindowCompositionAttribute` with `ACCENT_ENABLE_ACRYLICBLURBEHIND` (`platform/windows/window.rs:770`). On macOS it is an `NSVisualEffectView` (`platform/mac/window.rs:256`). **There is no Mica in GPUI.**
- Mica needs `DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW)` (Windows 11 22H2 or later) on the window's HWND, which the app can get through `raw_window_handle::HasWindowHandle` (implemented for `Window`, `window.rs:4845`). That is one `unsafe` FFI call through `windows-sys` (already a dependency; it needs the `Win32_Graphics_Dwm` feature). **It needs your approval (rule 2).** It also needs a spike: GPUI calls its own composition attribute when the background appearance is set, and the two may fight.
- Proposal: macOS uses `Blurred` for the sidebar and top bar. Windows 11 uses Mica if the spike works, else stays opaque (acrylic through an undocumented API, with known lag while dragging on some systems, is not worth it). Linux stays opaque. `chrome` is painted at 70 % alpha over the material and at 100 % when the system's transparency setting is off.

### 1.7 Icons and thumbnails

- **Glyphs**: twelve 16 px stroke icons (1.4 px stroke), one per kind (see the token board), drawn as SVG in `assets/icons/` and painted with GPUI's `svg()` element in the kind's tint, on its wash tile. This replaces today's single folder glyph. Kind comes from extension, then type code (the table in M5 §8), then "Other".
- **Thumbnails** replace the glyph tile when one exists: pictures, PDFs (first page), 3D models, fonts, and classic Mac custom icons (`icns`, `ICN#`, `icl8` from the resource fork when the Finder's custom-icon flag is set). On HFS volumes the real Mac icons are what make the list feel like the disk it is.
- **Pipeline** (`state/thumbnails.rs`, new): requests come only for rows in the visible range of the `uniform_list`, after scrolling has stopped for 60 ms. Decoding runs on the background executor, at most `THUMB_JOBS = 4` at a time, through the preview providers in a "thumbnail" mode with a maximum edge of 64, 128 or 256 px. JPEGs use their EXIF thumbnail when present. The cache holds at most `THUMB_CACHE = 512` entries or 48 MiB, keyed by `(SourceId, NodeId, size class, modified)`, and evicted least-recently-used. Eviction calls `cx.drop_image` (the M5 rule). Every result carries the listing generation.
- **Icon view** (planned for M8, pulled here): tiles of 96 (regular) or 128 (roomy) px, with name under them in two lines at most, and the same selection fill.

### 1.8 States

Each state says what happened and what to do next, with a small line drawing in `ink3` (no illustrations of people, no mascots). They are on the States board:

| State | Drawing | Text |
|---|---|---|
| Empty folder | A dashed coastline | "This folder is empty" / "Nothing is stored in Day 3 on Macintosh HD." |
| Loading a folder | none | Rows appear as they arrive; after 150 ms with nothing, "Reading this folder…" in `ink3` |
| Building the index | A survey sweep line moving across the disk strip | "Indexing Archive 2009" / "18,400 of about 48,000 folders. You can browse and search what is done." |
| Needs permission | A disk with a lock | "Samsung T7 needs permission" / "Windows lets only administrators read a whole disk. Disk Atlas asks once, through the Windows prompt, and only ever reads." [Allow reading disks] "What this allows" |
| Unsupported | The partition, hatched | "Disk Atlas cannot read APFS yet" / its size and place |
| Damaged | none | What was read, then a notice: "Part of this folder could not be read (catalog node 4127)." |
| Disconnected | none | The last listing at 45 % opacity under "Samsung T7 was disconnected. … Connect it again to keep browsing." |

## 2. Motion

### 2.1 Tokens (`theme::motion`)

| Token | Duration | Easing | For |
|---|---|---|---|
| `INSTANT` | 0 | none | Selection under key repeat, typing, results arriving |
| `QUICK` | 90 ms | ease-out cubic | Hover fade, button press, one-step selection glide, crossfade between Quick Look files |
| `STANDARD` | 160 ms | `ease_out_quint` | Details pane, sidebar groups, folder navigation |
| `EMPHASIZED` | 240 ms | `ease_out_quint` | Quick Look content opening, space-map zoom |
| exit | 0.7 × the entry | ease-in cubic | Every closing movement |

GPUI 0.2.2 has `Animation::new(duration).with_easing(..)`, `AnimationExt::with_animation` and `ease_out_quint()` (`elements/animation.rs:15`, `:52`, `:233`). There are no implicit transitions: each animated property is driven by an `AnimationElement` keyed by an id, which restarts when the id changes. That shapes the rules below.

### 2.2 Where it applies

- **Hover**: only the hovered row is wrapped in an animation (keyed by the row's id), fading `hover` in over `QUICK`. One animated element at a time, whatever the list length.
- **Selection**: the fill moves instantly (input latency first). A one-step move by keyboard, not repeated, glides a single "selection plate" drawn above the list between the two rows over `QUICK`. If that costs a frame on a 100,000-row list in the trace, the glide is dropped and selection stays instant. Spike first.
- **Folder navigation**: the new listing's container slides 12 px from the side you went (forward from the right, back from the left) and fades in over `STANDARD`. The old listing is not kept for a crossfade (it would double the rows drawn).
- **Sidebar groups**: height from 0 to rows × row height, `STANDARD`; the "N other partitions" row too.
- **Details pane**: width 0 to 300 over `STANDARD`, content clipped. The list relayouts each frame, which is fine for its visible rows only; this is checked in the trace.
- **Quick Look**: the window appears at once (the OS animates its own window); its content fades and scales from 0.97 over `EMPHASIZED`. Moving to another file crossfades the content over `QUICK` with an 8 px slide in the direction of travel. Closing: exit timing, then the window closes.
- **Progress**: determinate bars ease to each new value over `STANDARD`. The index build draws a sweep across the disk strip, one pass per 1.2 s.
- **Space map**: clicking a region zooms its rectangle to fill the map over `EMPHASIZED`, then the deeper level fades in. This is the one orchestrated moment in the app.

### 2.3 Reduced motion

GPUI 0.2.2 does not report the system setting (known since M5 task 6; today the app reads `DISK_ATLAS_REDUCED_MOTION`). Proposal: read it from the platform at startup and when the window regains focus. On Windows that is `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`, one `unsafe` call through `windows-sys`. On macOS it is `NSWorkspace.accessibilityDisplayShouldReduceMotion`, through `objc` (unsafe, and a new direct dependency, though already in `Cargo.lock` through GPUI). On Linux it is `gsettings get org.gnome.desktop.interface enable-animations` (a command, no `unsafe`). There is also a setting with three positions (System, On, Off), and the environment variable keeps working. With reduced motion on, slides and scales become a fade over `QUICK`, glides are instant, and the sweep is a static stripe. **The two `unsafe` calls need your approval.**

### 2.4 Budget

No animation may cost a frame. Proof per milestone: a traced timing (`--profile profiling`) of each animation on a 100,000-entry folder, with no frame over 16.7 ms, recorded in `performance.md`.

## 3. Disks and sidebar

### 3.1 Finding disks, and permission

Disks are already found at startup (`state/sources.rs`, `device::list`). What fails is opening them without administrator rights, and nothing says so. Proposal:

1. **Now (R3):** at startup, if any disk is `NeedsElevation`, the sidebar shows one notice under Disks: "N disks need permission before Disk Atlas can read them." with "Allow reading disks". Clicking a locked disk opens the Needs-permission state in the content area (no silent rows). The button uses the existing `elevation::restart_elevated` (UAC), which restarts the whole app elevated.
2. **Then (R9): the device helper**, already specified in `provider-protocol.md` §7 and §8.2 (M8 task 14), pulled forward. One UAC prompt starts `disk-atlas --device-helper`, which serves read-only `ListDevices`, `OpenDevice` and `ReadDevice` over a named pipe; the app itself never runs elevated, and drag and drop from Explorer keeps working (an elevated window does not accept drops from an unelevated Explorer). **This is privileged code taking requests and was already marked as needing your go-ahead.** Its four checks with a real prompt (§8.2) come first.

On macOS and Linux the notice shows `elevation::manual_hint()` until the helper exists there.

### 3.2 What the Disks group lists

Each physical disk appears once, with its volumes under it:

| Partition | Where it goes |
|---|---|
| Readable (Disk Atlas has a reader, it mounts) | Listed, in map order, with its format at the right |
| System partitions: EFI, Microsoft reserved, Apple recovery and boot, BIOS boot | Behind "N other partitions", even when readable (Recovery HD is readable and opens from there) |
| A format with no reader yet (APFS before M7, NTFS, ext before M8) | Behind "N other partitions", reason: "NTFS. Disk Atlas cannot read NTFS yet; Windows can, if you mount it." |
| Unknown content, or a partition that failed to mount | Behind "N other partitions", with the error in plain words |

- **A disk whose every volume the host already mounts** (on this machine, the Windows disk) moves out of Disks: its volumes stay under This computer, each with the disk's model as its subtitle ("Windows (C:)  Lexar SSD"). Mapping a drive letter to its physical disk takes `IOCTL_STORAGE_GET_DEVICE_NUMBER` on `\\.\C:` through the existing `ioctl` wrapper in `io/device/windows.rs` (no new `unsafe` block). macOS and Linux: by matching `/dev/diskN` or `/sys/block` to the mount table, no FFI.
- Disk rows: model name (never cut; wraps to two lines if it must), size, a 4 px usage bar of the partitions to scale. Volume rows: indented under the disk, the current one bold with a `pin` dot.
- Disks are collapsible; the current disk is open, the others closed at startup.
- "N other partitions" is a quiet 12 px `ink3` row with a chevron; opened, each partition shows its name and one line of reason. Hovering the closed row shows the same reasons in a tooltip.

### 3.3 The disk strip becomes a scale bar

Above the list: partitions to scale, as today, with the occupancy relief. Added: ticks with round sizes (0, 250 GB, 500 GB …) above it, the current volume outlined in `ink2`, others at 55 %, the selected file's pieces as `pin` marks, and a label "IMG_2041.JPG, 3 pieces" under the current volume.

### 3.4 Disk view (selecting a disk, not a volume)

Clicking a disk row (rather than a volume) shows the disk itself (Disk board): its name, connection, size and map scheme, a 56 px partition map with names and use inside each partition, a classic alternating scale bar under it, and the **space map** of the current or first readable volume.

### 3.5 The space map (the signature)

A squarified treemap of the volume, drawn as a map: regions are folders sized by bytes, filled with the wash of their dominant kind, outlined with borders whose weight falls with depth (2 px for the level shown, 1 px below). Free space is a hatched water region with an italic label. The region holding the selected file is outlined in `pin`. Click a region to go in (240 ms zoom), Backspace to go out, Enter to open it in the list. A third view mode next to list and icons (Ctrl+3).

- **Data**: folder sizes need the whole volume, which the M4 index already walks. **Verify first** whether `Index` keeps each entry's parent and size; if it does not, adding them changes a saved data layout, which is a stop-and-ask case. Aggregation runs on the background executor, one pass bottom-up.
- **Layout**: Bruls, Huizing and van Wijk's squarified algorithm, for two levels below the shown folder. Tiles smaller than 6 × 6 px merge into "N smaller items". At most `MAX_TILES = 2000` drawn per frame (`canvas` with `paint_quad`), labels only on tiles of at least 60 × 28 px.
- **Budgets**: aggregation of 1,000,000 entries ≤ 150 ms; layout ≤ 8 ms; zoom at 60 fps. Measured, not assumed.

## 4. File operations on the host filesystem

Rule 1 is lifted for the host filesystem only (done: `.claude/CLAUDE.md` rule 1 now says so). Every format Disk Atlas reads itself stays read-only.

- **Type-level guard.** Operations take a `HostPath` (newtype in `crates/app/src/ops/`), constructible only from a location whose source is `SourceKind::Host`. Nothing that takes a `NodeId` on a non-host source can reach an operation. A host path that is a disk image Disk Atlas has open is refused ("Mac OS 9.2.2.dmg is open in Disk Atlas. Close it first.").
- **Operations**: copy, move, rename, new folder, move to the Recycle Bin (Trash on macOS and Linux). There is no permanent delete in R8.
- **Recycle Bin**: the `trash` crate (MIT, 5.3.0, released 2026-11-05, 1.8 M downloads in 90 days), which uses `IFileOperation` on Windows, `NSFileManager` on macOS and the freedesktop trash on Linux. **New dependency, your approval needed.** The alternative is our own `IFileOperation` COM code, more `unsafe`, for no gain.
- **Copy and move**: chunked copy (1 MiB) on the background executor through the existing `Transfers` queue, so progress and cancel are shared with copying out. Move within one volume is `rename`; across volumes it is copy, verify the length, then delete the source. A cancelled copy removes its partial file.
- **Conflicts** ask, unlike copying out (README decision 4): "Replace", "Keep both" (" (2)", the existing `NameSet` rule), "Skip", with "Do this for all".
- **Undo** (Ctrl+Z), session-only, at most 50 steps: rename, move, new folder and copy (undoing a copy moves the copies to the Recycle Bin). Undoing "Move to Recycle Bin" uses `trash::os_limited::restore_all`, which exists on Windows and Linux; on macOS undo says "Restore it from the Trash in Finder."
- **Delete** asks: "Move 3 items to the Recycle Bin?" [Move to Recycle Bin] [Cancel].
- **Rename** in place: F2 or a slow second click; the stem is selected, not the extension; Enter commits, Escape cancels. **New folder**: Ctrl+Shift+N makes "New folder" (unique) and starts rename.
- **Drag and drop** inside the app: GPUI's `on_drag` and `on_drop` with a typed payload (`elements/div.rs:499`, `:462`). Targets are folder rows, host places in the sidebar and breadcrumb segments. It moves within a volume and copies across volumes; Ctrl forces copy. Dragging from a read-only volume onto a host folder copies out through the existing `extract::copy_out`. A read-only target shows "Macintosh HD is read-only" and refuses the drop.
- **From other apps**: drops of `ExternalPaths` (`interactive.rs:497`) onto a host folder copy in. On Windows, Paste can read files that Explorer copied (`CF_HDROP`, `platform/windows/clipboard.rs:348`); macOS and Linux need checking.
- **Limits of GPUI 0.2.2**: there is no drag out of the window to other apps (no `DoDragDrop` or dragging session in the source), and the app cannot put a file list on the system clipboard. Cut, copy and paste between Disk Atlas windows work through its own clipboard, and Copy also puts the paths on the system clipboard as text.
- After an operation, the folders involved are re-read. Watching folders for outside changes is not in R8.

## 5. Quick Look: bugs and layout

### 5.1 Space after clicking a tab (cause found)

Quick Look's tab buttons carry the `BUTTON` key context (`views/preview.rs:279`), where Space means `Press` (`keymap.rs:309`). Space to close is bound only in the file list's context (`keymap.rs:241`); the panel's context has Escape but not Space (`keymap.rs:261`). A click on a tab moves focus to the tab, the deepest context wins, and Space presses the tab again.

Fix: (1) Space and Escape are bound to `Close` on Quick Look's root context, and its buttons use a `QUICK_LOOK_BUTTON` context where only Enter presses; (2) clicking a tab or a toolbar button selects it without taking focus (no `track_focus` on mouse-down), so keyboard focus stays where it was; (3) the bytes view's "Go to byte" field is the only place in the window where Space is text. Test: a GPUI `TestAppContext` test that opens Quick Look, clicks the Bytes tab, dispatches Space, and asserts it closed, and the same for Escape and for each button.

### 5.2 Layout on the grid (Quick Look board)

- Header 56 px: 16 px in from the edge; a 32 px kind tile; name at 15/700; under it, in 12 px `ink2`, kind, dimensions or length, and size. The fork switch (Data | Resource) sits in that line when the file has a resource fork.
- Tabs: a segmented control centered in the header, 30 px segments, 13 px text, 16 px side padding (today: tiny tabs at the left).
- Toolbar at the right: Keep on top, Full screen, Copy to this computer (34 × 32 px buttons, 4 px apart), then the window controls.
- Content: 16 px side margins, the well on `well` with a 12 px radius and **no outline** (tone only). Images sit inside a 24 px inner margin with `ObjectFit::Contain` and are never enlarged past 2×. Nothing touches an edge.
- Footer 52 px: previous and next buttons with the neighbours' names in `ink3`, and "12 of 14 in Iceland 2014" in the middle. The floating metadata line is gone; what it said is in the header.

## 6. Quick Look as its own window

What GPUI 0.2.2 supports, read from the source:

| Need | GPUI 0.2.2 | Plan |
|---|---|---|
| A second window | `cx.open_window(WindowOptions, ..)` (`app.rs:943`); entities are app-wide, so both windows share `QuickLook` and `Listing` | Yes |
| Kind | `WindowKind::{Normal, PopUp, Floating}` (`platform.rs:1262`). On Windows, `Floating` is created exactly like `Normal`, and `PopUp` is a tool window without a frame or always-on-top (`windows/window.rs:394`). On macOS, `Floating` is `NSNormalWindowLevel` and `PopUp` is `NSPopUpWindowLevel` on all spaces (`mac/window.rs:779-813`) | `Normal`, with our own title bar like the main window |
| Full screen | `window.toggle_fullscreen()` (`window.rs:4122`), all platforms | Yes: button, F11 (Ctrl+Cmd+F on macOS) |
| Remembered position and size | `window.window_bounds()` (`window.rs:1466`), `cx.observe_window_bounds` (`app/context.rs:429`), `WindowOptions::window_bounds` | Yes; saved in the settings file (README decisions 13 and 14), per session until that exists |
| Always on top | **Not in GPUI.** Windows: `SetWindowPos(hwnd, HWND_TOPMOST, ..)`, one `unsafe` call through `windows-sys` (feature already enabled). macOS: `[window setLevel: NSFloatingWindowLevel]` through `objc`, `unsafe` and a direct dependency. Linux X11 needs `_NET_WM_STATE_ABOVE` through the X connection GPUI does not expose; Wayland forbids it | Windows and macOS: **your approval for the `unsafe` and for `objc`**. Linux: the toggle is hidden |

Behaviour:

- Space or Enter on a file opens the window; Space or Escape in the window closes it, whatever has focus (§5.1).
- Left/Right and Up/Down in the Quick Look window move the main window's selection to the previous or next file and the window follows (the M5 debounce of 30 ms stays). Home and End go to the first and last.
- Arrow keys in the main window keep moving its selection, and the open Quick Look window follows, as today.
- One Quick Look window at a time. Closing the main window closes it.
- The window does not take focus when it opens from the keyboard on Windows, if GPUI allows `focus: false` there (`WindowParams::focus` is marked unused on Windows, `platform.rs:1169`: verify; otherwise focus moves to it and its keys mirror the list's).

## 7. Quick Look: what file types

Dependency costs are measured: each crate was resolved alone and its tree compared with today's `Cargo.lock` (2026-11-13). "New" counts crates not already built. Licenses and dates are from crates.io on the same day.

| Format | Approach | New crates | Caveats | Phase |
|---|---|---|---|---|
| PNG, JPEG, GIF (animated), BMP, ICO, TIFF, WebP, TGA, DDS, PNM, QOI, HDR, EXR | `image` 0.25.10, already built with these features through GPUI; GPUI's `img` animates multi-frame images (`elements/img.rs:314-324`) | 0 | Animated WebP: check `image-webp` frame support; else first frame | Q1 |
| SVG | `resvg`/`usvg` 0.45.1, already in the tree through GPUI. External files refused: `resources_dir: None` and an `image_href_resolver` that refuses everything (`usvg` `Options`) | 0 | Lifts M5's "SVG is never previewed" once these two options are in place and tested with a hostile file | Q1 |
| PICT (classic Mac) | In-house QuickDraw subset: version 1 and 2 headers, `PackBitsRect`, `DirectBitsRect`, `PackBitsRgn`, and QuickTime JPEG (opcode `0x8200` into `zune-jpeg`, in the tree) | 0 | Lines, shapes and text opcodes are not drawn in Q1; the preview says so | Q1 |
| MacPaint (`PNTG`) | In-house: 576 × 720, 1 bit, PackBits | 0 | | Q1 |
| Classic Mac icons (`ICN#`, `icl8`, `ics#`, `cicn`, custom icons) | In-house resource-map reader (bounded), also the base for a later resource browser | 0 | | Q1 |
| ICNS | Q1: in-house for PNG and RLE ARGB entries. Q2: `icns` 0.5.0 (MIT, 2026-09) for JPEG 2000 entries | 0 / 3 | | Q1, Q2 |
| PSD | In-house: the merged composite in the image data section (raw or RLE) | 0 | `psd` crate last released 2024-01; a file saved without "maximize compatibility" has no composite and falls back | Q1 |
| Camera RAW | Q1: in-house, the embedded full-size JPEG of TIFF-based RAWs (CR2, NEF, ARW, DNG, ORF, RW2, PEF) and CR3. Q3: `rawler` 0.8.0 (LGPL-2.1, compatible) for a true decode | 0 / 29 | | Q1, Q3 |
| HEIC, HEIF | Platform decoders: WIC on Windows (needs the Store's HEIF and HEVC extensions, the latter paid on some editions), ImageIO on macOS; Linux: `libheif-rs` 3.0.0 (MIT) if `libheif` is installed | 0 / 4 + system lib | `unsafe` COM and `objc` | Q3 |
| AVIF | Platform as for HEIC (WIC with the AV1 extension; macOS 13+), or `rav1d` 1.1.0 (BSD-2, 6 new, last release 2025-05) | 0 / 6 | Decide after measuring `rav1d`'s compile time | Q3 |
| JPEG XL | `jxl-oxide` 0.12.6 (MIT/Apache) | 16 | | Q3 |
| PDF, multi-page | `hayro` 0.9.0 (pure Rust, Apache/MIT, 2026-11-02): pages rendered on the background executor, one at a time, a page list down the side | 27 | Fidelity checked on 20 real PDFs first. Fallback: `pdfium-render` (5 crates, but ships a ~6 MB PDFium binary per platform and runs C++ on hostile files) | Q2 |
| Markdown | `pulldown-cmark` 0.13.6 (MIT) into GPUI text: headings, lists, emphasis, code, tables. No remote images, links not followed | 1 | | Q1 |
| RTF | In-house control-word stripper, bounded, to paragraphs | 0 | `rtf-parser` pulls WebAssembly glue (`tsify`, `gloo-utils`): rejected | Q1 |
| Office (docx, xlsx, pptx, odt, ods, odp) | In-house: zip central directory, `flate2` (in tree), `quick-xml` (in tree). Shows the file's own thumbnail when it has one (`docProps/thumbnail.*`, ODF `Thumbnails/thumbnail.png`), else a text outline. Q2: spreadsheets as a grid through `calamine` 0.38.0 (MIT), which also reads `.xls` | 0 / 9 | Legacy `.doc` and `.ppt` fall back | Q1, Q2 |
| Text and code, highlighted | `syntect` 5.3.0 with `default-fancy` (pure-Rust regex), plus `two-face` 0.5.2 for more grammars | 11 + 4 | `tree-sitter` rejected: needs a C compiler and one crate per language. Highlighting on the background executor, within `MAX_TEXT_BYTES` | Q2 |
| JSON, XML, plist (XML and binary) pretty-printed | `serde_json` and `quick-xml` (both in tree); binary plist in-house (bounded) | 0 | `plist` crate costs 6 | Q1 |
| Audio: waveform, tags, cover art | `symphonia` 0.6.1 (MPL-2.0, compatible) decoded to peaks; `lofty` 0.25.5 (MIT/Apache) for tags | 22 + 4 | Check `symphonia`'s licence files for an "Incompatible With Secondary Licenses" notice before approval | Q3 |
| Audio playback | `rodio` 0.23.0 / `cpal` (MIT/Apache) | 18 (shares `symphonia`) | Linux needs ALSA headers to build | Q3 |
| Video, with scrubbing | Platform: Media Foundation `IMFSourceReader` (Windows), AVFoundation (macOS), GStreamer optional on Linux. Poster frame, duration, codec, then scrubbing through seeks | 0 Rust crates on Windows (`windows-sys` features) | Most `unsafe` of all; codecs are whatever the OS has; `ffmpeg-next` rejected (ships FFmpeg libraries, size, patents) | Q3, last |
| Archives, listing | In-house: zip central directory, tar and tar.gz (`flate2`), RAR 5 headers. Q2: 7z through `sevenz-rust2` 0.25.0 (Apache-2.0) | 0 / 4 | `unrar` rejected: as far as I know, its bundled UnRAR source licence is not GPL-compatible (unchecked). Listing only | Q1, Q2 |
| MacBinary, BinHex, AppleSingle/Double | In-house (`structure.rs` already recognises them): show the inner file's name, type, creator and forks | 0 | StuffIt later | Q2 |
| Fonts | Specimen: outlines from `ttf-parser` (in tree) filled with `tiny-skia` (in tree) into an image. Never handed to GPUI's text system: font bytes from a disk are hostile. Classic Mac `FFIL` suitcases: their `sfnt` resources, same path | 0 | | Q1 |
| Folders | Contents by kind, total size (from the index when built), largest items, a small space map | 0 | | Q1 |
| 3D models | Parsers in-house (§7.1); rendered on the GPU through `wgpu` (§7.2) | 16 (`wgpu`) | Child window on Windows and macOS, readback on Linux | Q1 parsers, G1 viewer |
| Anything else | The kind glyph at 96 px, the facts, and the Bytes tab | 0 | | Q1 |

**Build first:** the Q1 rows. Apart from the 3D viewer (G0 and G1), they cost no dependency, cover the formats a classic Mac disk is full of (PICT, MacPaint, icons, fonts, MacBinary), and every new parser in them gets a fuzz target. Q2 adds one approved crate at a time (PDF first). Q3 is platform work and heavy crates, each its own approval.

### 7.1 3D models

- **Formats**: STL (ASCII and binary), OBJ (geometry; materials ignored in the first version), PLY (ASCII, binary little- and big-endian), OFF, glTF 2.0 and GLB (meshes and base colours; textures later) through `serde_json` (in tree), and 3MF (zip plus XML, with colours). USDZ shows its embedded thumbnail if any; FBX, STEP, IGES and Blender files fall back (STEP is text, so its Preview is the text).
- **Parsers** are in-house and bounded: `MAX_MODEL_FILE = 512 MiB`, `MAX_TRIANGLES = 20_000_000`, `MAX_VERTICES = 20_000_000`; indices checked against the vertex count; glTF buffers only from GLB's own chunk or from sibling files reached through the volume's `FileSystem` (never a host path for a non-host volume, never a URL). One fuzz target per format, with seeds.
- **Facts**: triangle count, size of the bounding box in file units (millimetres assumed for STL and 3MF, said so), watertight or not (edge check, capped).
- **Interaction**: drag to turn, scroll to zoom, Shift-drag to move, double-click to reset; Shaded and Wireframe; perspective or orthographic. Frames are drawn only when something changes (input, resize, a chunk of the model arriving), never continuously.

### 7.2 The 3D renderer runs on the GPU

Julian's instruction (2026-11-13): the viewer uses the GPU, so the biggest models a PC can hold turn smoothly. Research, from the GPUI 0.2.2 source and the crate registry on 2026-11-13:

- **GPUI has no hook for outside GPU work.** Its scene holds only quads, shadows, paths, underlines, sprites and macOS surfaces (`scene.rs:199`). The `surface()` element exists only on macOS and only takes biplanar YCbCr video frames (`mac/metal_renderer.rs:1087`), so it cannot carry RGBA 3D output. The renderer's device is private on every platform. `gpui` 0.2.2 is still the newest release on crates.io.
- **`gpui_interop`** (Apache-2.0, 0.2.1, 2026-11-03) does exactly this (shared textures, no CPU copy), but for the `bite-gpui` fork, not Zed's `gpui`. It is marked provisional and had 988 downloads in 90 days. Using it means changing GPUI, which is a bigger decision than this feature: rejected for now, noted as the path if we ever move GPUI.
- **GPUI's Windows renderer leaves room for a child window.** By default it presents through DirectComposition: a premultiplied-alpha swap chain (`windows/directx_renderer.rs:1041`) on a composition target made with `CreateTargetForHwnd(hwnd, true)` (`:793`). The `true` puts GPUI's visuals above the window's child windows. So a child window placed under a region GPUI leaves transparent shows through, and GPUI's own controls drawn over that region stay on top. (When `ZED_DISABLE_DIRECT_COMPOSITION` is set, GPUI uses a plain window swap chain instead and this does not hold: the fallback below applies.)
- **`wgpu`** (MIT or Apache-2.0, 30.0.2, released 2026-11-12, 18.8 M downloads in 90 days) is the renderer: Direct3D 12 or Vulkan on Windows, Metal on macOS, Vulkan on Linux, from one WGSL shader set. Measured cost: **16 new crates** with default features, 15 with Direct3D 12 only (`naga` and most of the rest are already built). Compile time to be measured before approval.

**Design: one `wgpu` renderer, two ways of putting its frames on screen.**

| Presenter | Where | How | Copy per frame |
|---|---|---|---|
| **Direct**: a child window | Windows with DirectComposition (the default), then macOS after a spike | Windows: a `WS_CHILD` window created inside the Quick Look window's HWND (from `HasWindowHandle`), covering the viewport rectangle and moved with it on every layout. Its window procedure answers `WM_NCHITTEST` with `HTTRANSPARENT`, so mouse and wheel input go to GPUI's element as usual. `wgpu` makes its surface on that child. The Quick Look view paints nothing under the viewport (backgrounds are drawn around it, not behind it), so the 3D frame shows through, and the overlay controls (Shaded/Wireframe, Reset view, facts) are ordinary GPUI elements on top. macOS: an `NSView` with a `CAMetalLayer` added below GPUI's view, GPUI's layer made non-opaque; to be proven in a spike | None: the GPU presents directly |
| **Readback** | Linux, Windows without DirectComposition, macOS until its spike passes, and every thumbnail | `wgpu` renders to an offscreen texture; the frame is copied into a mapped buffer (two buffers in turn, so the GPU never waits) and becomes a `RenderImage`; the previous one is released with `drop_image` | One frame, about 3.7 MB at 1280 × 720; one frame of latency |

Both presenters draw the same scene with the same code. With no usable GPU adapter, `wgpu`'s fallback adapter is tried (WARP on Windows, which every Windows 10 and 11 has; llvmpipe on Linux when installed); without one, the preview says "3D previews need a graphics driver." and shows the facts. The CPU rasterizer of the first draft is dropped.

**Rendering big models fast:**

- **Load off the UI thread, stream to the GPU.** Parsing runs on the background executor, with cancel checks. Vertices are welded (hash of the position) so STL's three-vertices-per-triangle becomes an indexed mesh. The mesh is uploaded in chunks as it is parsed, so a large file starts drawing (bounding box first, then the parts as they arrive) before it has finished loading.
- **Little memory per vertex.** Positions as `f32 × 3` and `u32` indices: 12 bytes per vertex plus 12 per triangle. Flat shading takes the face normal from screen-space derivatives in the fragment shader (`dpdx`/`dpdy`), so no normal buffer is needed. Smooth shading adds octahedral normals (4 bytes per vertex) only when the file has normals. 20,000,000 triangles with about 10,000,000 vertices is about 360 MB of GPU memory.
- **Respect the adapter's limits.** Buffers are split at `min(adapter.limits().max_buffer_size, 128 MiB)` and drawn as several draw calls. GPU memory per model is capped at `MAX_MODEL_GPU_BYTES = 1 GiB` or a quarter of what the adapter reports, whichever is smaller. A model over the cap is decimated on the CPU (vertex clustering) before upload, and the facts say "shown simplified".
- **Draw only what changed.** Frames are drawn on input, resize or new data, with FIFO presentation (vsync). While the user turns the model, nothing else is computed. 4× MSAA. Wireframe uses `PolygonMode::Line` where the adapter has it, else a generated line index buffer.
- **The model never reaches the GPU unchecked.** Every index is checked against the vertex count on the CPU before upload. Shaders are our own WGSL, compiled once at startup. `wgpu`'s validation and robust buffer access stay on.

**Budgets** (release build, measured with GPU timestamp queries and traced spans, recorded in `performance.md`):

- A 1,000,000-triangle binary STL: parsed and welded in ≤ 400 ms, first frame ≤ 500 ms after Space.
- Turning a 20,000,000-triangle model at ≥ 60 fps at 2560 × 1440 on a mid-range discrete GPU (an RTX 3060 or similar). On integrated graphics, 5,000,000 triangles at ≥ 60 fps.
- The UI thread spends ≤ 2 ms a frame on the viewer (moving the child window and handing over input).

**`unsafe` and dependencies this needs (your approval):** `wgpu` itself. On Windows, creating the child window: `RegisterClassExW`, `CreateWindowExW`, `SetWindowPos`, `DestroyWindow` and a window procedure, through `windows-sys` (already a dependency), and `wgpu`'s `create_surface_unsafe` with the child's handle. On macOS, the `NSView` and `CAMetalLayer` calls through `objc` (decision 20). All of it lives in one module (`crates/app/src/viewport/`), written by O, each block with `// SAFETY:`.

**Spike first (G0, before G1):** a child window under a transparent GPUI region in a real window on Windows 11: does it show, do GPUI controls stay on top, do resizing and moving keep it in place without tearing, does `HTTRANSPARENT` hand input to GPUI, and what happens with two monitors at different scales. If any of it fails, Windows falls back to the readback presenter, which still renders on the GPU.

- **Thumbnails**: the same renderer, offscreen at 256 px, through readback.

## 8. Rules and docs that change

- `.claude/CLAUDE.md` rule 1: **done** (2026-11-13), host filesystem operations allowed, everything Disk Atlas reads itself stays read-only.
- `ui-design.md`: §1 to §2 replace Tokens, Metrics and Motion on approval; §3 to §6 replace the sidebar, disk strip and Quick Look sections.
- `.claude/rules/app-ui.md`: add "Motion comes from `theme::motion`; no literal durations. Every animation has a reduced-motion form."
- `performance.md`: budgets from §2.4, §3.5, §7.1 and thumbnails (first visible thumbnails within 150 ms of scrolling stopping; no frame over 16.7 ms while they arrive).

## 9. Milestones

Each is small enough to review. **Gate for every UI task: a screenshot of the running app showing the result, in both themes, attached to the review; the lead looks at it before accepting. Tests alone do not close a UI task.** The lead's screenshot tool (a script that captures the app window and can click and type) moves into `tools/` with R1.

| # | Milestone | Owner | Depends on |
|---|---|---|---|
| R1 | Quick Look keys fix (§5.1) with tests; layout on the grid (§5.2), still as a panel | S, lead reviews | approval |
| R2 | Tokens: colours, kinds, type scale, spacing, densities, elevation (§1.2–1.5); kind glyphs; every component moved to tokens; states (§1.8) | S, H for the mechanical part | R1 |
| R3 | Sidebar and disks: grouping, other partitions, host-mounted disks to This computer, permission notice and state, Restart as administrator (§3.1 step 1, §3.2–3.3) | S; O for the drive-letter mapping | R2 |
| R4 | Motion system (§2) and reduced motion; frame traces | O (the selection plate and list costs), S | R2; `unsafe` approval for §2.3 |
| R5 | Quick Look window (§6), keep on top, full screen, remembered bounds | O (platform calls), S (view) | R1; approval for §6's `unsafe` and `objc` |
| R6 | Thumbnails and icon view (§1.7) | O pipeline, S views | R2 |
| R7 | Disk view and space map (§3.4–3.5) | O layout and data, S view | R3; the index check in §3.5 |
| R8 | Host file operations (§4) | O operations and undo, S UI | R2; `trash` approval |
| R9 | Device helper (§3.1 step 2; `provider-protocol.md` §8.2) | O | your go-ahead; the four prompt checks |
| Q1 | Previews without new crates (§7); the 3D model parsers and their fuzz targets | S parsers, O PICT | R5 |
| G0 | Spike: a `wgpu` child window under a transparent GPUI region on Windows 11, then macOS (§7.2) | O, lead looks at it running | `wgpu` and `unsafe` approval |
| G1 | The GPU 3D viewer: `wgpu` scene, both presenters, streaming upload, limits, budgets measured (§7.2) | O | G0, Q1's parsers |
| Q2 | Previews with one crate each: PDF, highlighting, ICNS JPEG 2000, 7z, spreadsheets | S | each crate's approval |
| Q3 | Audio, video, HEIC, AVIF, JPEG XL, full RAW | O | each approval |

M6 (classic HFS) waits until R3, so the classic Mac previews of Q1 land on a sidebar and list worth looking at.

## 10. Decisions that are Julian's

Numbered after the sixteen in `README.md`. **Julian approved all twelve on 2026-11-13.** In 23, each Q3 crate is still asked for on its own when its turn comes, as the row says.

| # | Decision | Approved |
|---|---|---|
| 17 | The visual direction: survey chart, cool paper by day, deep water by night, magenta overprint | Yes, as on the boards |
| 18 | Where the sidebar puts host-mounted disks | Under This computer, not Disks |
| 19 | `unsafe` on Windows: Mica (`DwmSetWindowAttribute`), keep on top (`SetWindowPos`), reduced motion (`SystemParametersInfoW`) | Yes, each with `// SAFETY:`, written by O |
| 20 | `objc` as a direct dependency on macOS (already in `Cargo.lock` through GPUI), for keep on top and reduced motion | Yes |
| 21 | The device helper pulled forward from M8 (privileged code) | Yes, as R9, after its prompt checks |
| 22 | `trash` for the Recycle Bin | Yes |
| 23 | Preview crates | Q1 none. Q2: `hayro`, `syntect` + `two-face`, `icns`, `sevenz-rust2`, `calamine`, `pulldown-cmark` (Q1, 1 crate). Q3 each on its own |
| 24 | The space map reads sizes from the search index, building it if needed | Yes; stop-and-ask if the index lacks sizes or parents |
| 25 | SVG previews, with external references refused (lifts an M5 rule) | Yes |
| 26 | No permanent delete in R8, only the Recycle Bin | Yes |
| 27 | The 3D viewer on the GPU through `wgpu` (16 new crates), with a child window on Windows and macOS (`unsafe`) and readback elsewhere (§7.2) | Yes, after the G0 spike |
| 28 | Staying on Zed's `gpui` 0.2.2 rather than a fork with a GPU texture hook (`gpui_interop` needs `bite-gpui`) | Stay; revisit only if the child window fails on both platforms |

## 11. Changes during build

- **2026-11-14, R4 (O): motion system built; lead to review.** Decisions taken while building, for the lead to confirm:
  - **Reduced motion inputs.** `theme::motion::reduced(cx)` resolves, in order: `DISK_ATLAS_REDUCED_MOTION` (read once at startup; `0`/`false` forces full motion), the existing `motion` setting (`system`/`reduced`/`full`, the three positions of 2.3), then the system's answer. The system is asked at startup and on every window activation, on the background executor (`platform/reduced_motion.rs`): Windows `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)` (decision 19); Linux `gsettings get org.gnome.desktop.interface enable-animations` with a 500 ms timeout (not compiled or run here: Windows host); macOS not asked (TODO for decision 20, `objc` not added). Components that read it once at creation (menus, the Quick Look panel) keep that answer until reopened.
  - **Hover** no longer uses GPUI's `.hover()` style on file rows: the list tracks the hovered row with `on_hover` and fades its fill over `QUICK`. A row counts as hovered only while the pointer is also over it by geometry (list bounds and scroll of the last frame), so after a wheel scroll no row is lit until the pointer moves. Under reduced motion the hover fade stays (it is already a fade over `QUICK`).
  - **Selection plate**: spiked and kept (trace in `performance.md`). Drawn under the rows, so the destination row's text turns at once and its own fill is left to the plate while it glides. It glides only for Up/Down without Shift, between two rows both fully on screen, and not when the previous step was under `GLIDE_REPEAT_GAP` (2 × `QUICK`, 180 ms) before: a held key glides on its first press only. Quick Look's Up/Down (which move the list from `app.rs`) do not glide.
  - **Folder navigation direction** comes from crumb depth (deeper or sideways: from the right; shallower: from the left), not from Back/Forward, because `NavigationEvent` carries no direction and `navigation.rs` was not in scope. Back to a deeper folder therefore slides from the right. The slide starts when the new folder first draws something (rows or a state), not at the click, so a slow folder does not spend its slide on an empty list.
  - **Sidebar folds** grow to the content's height as measured on the last frame it was drawn (a canvas in the fold), not rows × row height, because "other partitions" items are two lines tall. Both folds close with the exit timing.
  - **Progress**: the only determinate bar in the app is R15's storage bar, which already eases over `STANDARD`; nothing new was needed. The index-build sweep on the disk strip is not built (no strip sweep exists yet).
  - **Menus** under reduced motion now fade over `QUICK` instead of appearing at once (every animation has a reduced form).
  - **Not changed**: Quick Look (`QUICK_LOOK_FADE`, 120 ms, R5) and the space map (R7). Timing constants that are not motion (`LOADING_DELAY`, Quick Look `DEBOUNCE`, search `PROGRESS_INTERVAL`, sizes `RESORT_INTERVAL`/`KEY_GUARD`, storage bar `LEGEND_INTERVAL`, type-to-select timeout) stay where they are.
  - **Frame trace**: `DISK_ATLAS_FRAME_TRACE=1` logs per-frame UI-thread time and interval (`crates/app/src/frame_trace.rs`), used for 2.4.
- **2026-11-15, R7 and R15.2 (lead, Sonnet): space map and disk view built.** Seen in both themes (dark: host folder map, zoom into a region, Backspace out, disk view and Layout on an HFS+ image; light: host folder map, disk view and Layout on `compressed.img`). Decisions taken while building:
  - **Engine** (`crates/lib/src/sizes/map.rs`, written before this session, two test fixtures fixed): `index_map`, `host_map`, `squarify`, `layout`. Timings in release, median of 7: layout of 705 tiles 0.95 ms (budget 8), `index_map` over 1,000,001 entries 11.5 ms (budget 150).
  - **The map follows the navigation.** It draws the folder the list would draw; going into a region navigates (`Location::with_crumbs` from `Index::crumbs` or from `fs.lookup` down a host path), so the breadcrumb, Back and Backspace all work. There is no separate zoom stack. `state/view.rs` (`ViewState`: `ViewMode::{List, SpaceMap}`, the disk view, Layout mode, an emphasised kind) and `state/space_map.rs` (`SpaceMap`: the tree, off the UI thread, dropped for a folder the user has left; a host folder still being measured is read again at most twice a second).
  - **Disk view** (`views/disk_view.rs`): the header (name, disk or image, size, map scheme) over the same map. The partition bar stays the storage bar above it; the 56 px bar with names and use inside each partition, and the alternating scale bar of 3.3, are **not built**: the storage bar already names each partition and its use beneath it. Clicking a disk heading opens the view; the chevron alone folds. A navigation the map did not ask for ends the disk view.
  - **Layout mode** (R15.2): the Contents | Layout switch now shows in the disk view only, and the mode is `ViewState::layout_mode`. "Show where it is on the disk" is in the menu of a file on a disk Disk Atlas reads; "Show on the space map" is in a file's menu; View has List and Space map. `Built::space_map` and `Built::layout_mode` are on. Keys: Ctrl+1 list, Ctrl+3 map (Cmd on macOS), Backspace and Enter inside the map.
  - **Free space** is drawn at the top of a volume or a drive, not of a folder such as Home.
  - **Zoom** (EMPHASIZED): the old picture zooms until the clicked region fills the map, then the new tree takes over. Zoom out and the fade-in of the deeper level are not animated (the new picture appears). Reduced motion: no zoom. **Not seen:** the zoom itself in motion (the screenshot tool cannot catch 240 ms).
  - **Not built:** arrow-key movement between regions, the right-click menu on a region (Open in the list, Zoom in, Copy path, Properties), a click on a file region (only folders go in), the System region (catalog and journal bytes the files do not account for), and the map being remembered per source. Carried to a later pass.
  - **Debug hook:** `DISK_ATLAS_OPEN_IMAGE=<path>` (debug builds) opens an image at start, so a screenshot run needs no file dialog.
