# Interface design

Reference mockups: https://claude.ai/artifact/Bo2fhD1PjQeq3qdjjtAW59 (three screens: Explorer, Search, Quick Look in the night theme). The mockups are the visual target; this file is the specification to build from.

**Since 2026-11-13 two approved designs change this file, and where they disagree with it, they win:**

- `design/r1-redesign.md` §1 to §6 replace Direction, Tokens, the sidebar and disk strip parts of Layout, and Quick Look. §1 and §2 are folded into this file (R2, 2026-11-13): Direction, Tokens and Motion below are authoritative. The pixel sizes in the Layout sketch below are the old ones; take bar, sidebar, details and row sizes from Tokens (§1.4).
- `design/r10-everyday.md` adds context menus (§6), Properties (§7), Settings (§8), the storage bar that replaces the disk strip above the list (§4), folder sizes in the Size column (§3.4), and the hidden and housekeeping rule (§5.1). Its boards: https://claude.ai/artifact/St9vwbp8Ber5eQuB79CekT

## Direction

> Source: r1-redesign.md §1-2, folded 2026-11-13; this file is now authoritative.

### 1.1 Direction: a survey chart of the disk

Disk Atlas draws a disk the way a survey chart draws ground: to scale, in layers, with a legend. Four ideas come from that and nothing else is decoration:

1. **Chart paper and deep water.** By day the surfaces are a cool, slightly green paper. By night they are deep blue-green water. No neutral grays; the hue is what makes it this product and not every dark app.
2. **The overprint.** On nautical and aeronautical charts, magenta is the overprint: the navigator's own marks on top of the printed chart. Disk Atlas keeps magenta for exactly that: what the user selected and where it lives on the disk. It is never a button, a hover or a decoration. This keeps the existing rule and gives it a reason.
3. **A legend of kinds.** Every file kind has one glyph and one muted tint, like land-use classes on a map. They tint list icons, icon tiles and the space map. None is pink, so the overprint stays unique.
4. **Scale and borders.** The disk strip becomes a scale bar with real ticks. The space map draws folders as regions, with higher-level borders drawn thicker, as country borders are. Free space is drawn as water, hatched, its label in italic (the cartographic convention for water names).

What stays from `ui-design.md`: one accent with one meaning, Atkinson Hyperlegible (chosen for look-alike characters in old names), italic for aliases, tabular figures, plain-word copy, the keyboard map.

Smaller conventions that come from the subject: links and aliases are set in italic (as the classic Finder did); a resource fork is a column, not a badge; sizes and dates use tabular figures.

## Tokens

> Source: r1-redesign.md §1-2, folded 2026-11-13; this file is now authoritative.

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

All text pairs meet 4.5:1 contrast. Keep it that way when adding tokens.

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

Both Atkinson Hyperlegible faces are SIL Open Font License; bundle them in `assets/fonts` and confirm the license file ships with them.

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

## Motion

> Source: r1-redesign.md §1-2, folded 2026-11-13; this file is now authoritative.

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

## Layout

```
+--------------------------------------------------------------------------+
| <  >   Macintosh HD > Users > julian > Projects      [ Search ]  = ::  _ [] x |   top bar, 44
+------------+-------------------------------------------------------------+
| Disks      |  Samsung T7  931.5 GB, read-only        | Selected file, 3 pieces |
|  Samsung T7| [|######|###  | ## ..........][ ##########....][ - - ]       |   disk strip
|  * Mac HD  |  Macintosh HD 252.7 of 465.1   Time Machine      Free        |
|    Time M. +----------------------------------------------+--------------+
|    EFI     | Name            Modified     Size  Rsrc  Kind |  preview     |
| Disk images| Archive 2019    12 Jan 2020               Fol.|  name        |
|  ...       | [Screen Shot 2014-03-02.png ... selected row ]|  details     |
| This comp. | ...                                           |              |
|            | 17 items   212.4 GB free   index up to date   |  [Copy to..] |
| [Open img] |                                               |  [Quick Look]|
+------------+-----------------------------------------------+--------------+
```

- **Top bar** (`components/titlebar.rs`): back, forward, the path as plain clickable segments (`components/breadcrumb.rs`), the search field, list and icon view buttons, window controls. There is no separate title bar or toolbar row.
- **Sidebar** (`components/sidebar.rs`): three groups (Disks, Disk images, This computer). Each device lists its volumes with the filesystem name at the right. The current volume is bold with an accent dot. "Open disk image" at the bottom; where the system file prompt cannot pick files and folders together (Windows), "Open disk image folder" sits directly under it for sparse bundles.
- **Disk strip** (new, `components/disk_strip.rs`): device name and size, the to-scale bar, a label under each partition. Clicking a partition opens it. The current volume is at full strength, others are dimmed. When a file is selected, its extents are drawn as accent marks. Unallocated space is a dashed outline.
- **File list** (`views/explorer.rs`, `components/file_row.rs`): columns Name, Modified, Size, Resource fork (HFS family only), Kind. No stripes, no row lines. The footer line under the last row carries item count, free space and index state (`components/statusbar.rs`).
- **Details pane**: preview well, name, a short list of facts in plain words, then the two actions. "Copy to this computer" is the primary action of the whole app and the only filled button.

## Search

Search is not a floating palette. Typing in the search field turns the file list into results in place: same rows, same columns plus a Folder column, same selection and keyboard behaviour, so results can be previewed and copied like any other file.

- Results update on every keystroke. The count and time show inside the field.
- Filters are plain menus under the top bar (where, kind, size, date). An active filter is filled with `ink`. No query syntax is required.
- The matching part of each name is bold.
- Escape clears the search and returns to the folder the user was in.

## Quick Look

Space opens a floating panel over the window with three views: Preview, Bytes, Details. On filesystems with forks, a fork switch sits above the content. The Bytes view shows offset, hex and text columns; selecting bytes that belong to a known structure explains them in the side column in plain words.

## Keyboard

| Key | Action |
|---|---|
| Arrows, Home, End, Page Up, Page Down | Move selection |
| Typing letters | Jump to the first matching name |
| Enter | Open a folder; on a file, open Quick Look |
| Ctrl+Enter (search results) | Show the file under the cursor in its folder |
| Backspace, Alt+Up | Enclosing folder |
| Alt+Left, Alt+Right | Back, forward |
| Space | Quick Look: open or close the panel (in the list; a space that continues a name being typed stays part of the name) |
| Ctrl+Tab, Ctrl+Shift+Tab | Next, previous Quick Look tab (Ctrl on every platform) |
| Tab, Shift+Tab (panel open) | Move between the panel's controls (tabs, fork switch, close); focus stays in the panel |
| Up, Down, Home, End (focus in the panel) | Move the selection in the list; the panel follows |
| Page Up, Page Down, Left, Right (focus in the panel) | Scroll the text preview |
| Ctrl+F | Search this volume |
| Down, Enter (in the search field) | Move to the first result |
| Escape | Leave search (clears the field), close Quick Look; in an empty field, back to the list |
| Enter, Space (on a filter control) | Open its menu; in the menu Up/Down move, Enter/Space choose, Escape closes the menu only, Tab moves to the next control |
| Up, Down, Home, End, Left, Right, Enter, Space, Escape, a letter (in any menu) | Move, open or close a submenu, choose, close one level, jump to the next item starting with the letter (R11; `menu::` actions in the `Menu` context) |
| Ctrl+L | Edit the path as text |
| Ctrl+1, Ctrl+2 | List view, icon view |
| Ctrl+I | Show or hide details. **From R18:** Properties, and details moves to Ctrl+Shift+I |
| Alt+Enter | Properties (R18) |
| Menu key, Shift+F10 | Open the context menu of the item under the cursor (`menu::Open`, built in R11.2): in the file list at the cursor row, in the search field under it, in the sidebar for the place being browsed (at the pointer). A Mac keyboard has no Menu key, so there it is Shift+F10 only. A right click does the same at the pointer; the breadcrumb has a menu for the mouse only |
| F5 | Refresh this folder and its sizes (R11; sizes arrive with R13) |
| Shift+Enter | Open a file of this computer in its app (R11; `file_list::OpenInApp`) |
| Ctrl+, | Settings (R10b) |
| Ctrl+Shift+. | Show or hide hidden items (R16) |
| Ctrl+3 | Space map view (R7) |
| F2, Delete, Ctrl+C, Ctrl+X, Ctrl+V, Ctrl+Z, Ctrl+Shift+N | Rename, move to the Recycle Bin, copy, cut, paste, undo, new folder: this computer only (R8) |
| Escape, Ctrl+W (Properties, Settings) | Close the window (R5) |
| Ctrl+Shift+C | Copy to this computer |
| Ctrl+O | Open disk image |
| Ctrl+Shift+O | Open disk image folder (sparse bundle) |
| Ctrl+G | Go to byte (Bytes view) |

On macOS, Ctrl becomes Cmd. The rows marked with a milestone are specified in `design/r10-everyday.md` §6.3, which also says what each was checked against in `keymap.rs`.

## States

Every state says what happened and what to do next, in the interface's voice.

- **Loading a folder:** rows appear as they arrive; no spinner unless nothing has arrived after 150 ms.
- **Empty folder:** "This folder is empty."
- **Unsupported format:** "Disk Atlas cannot read this partition yet (Core Storage)."
- **Encrypted volume:** a password field in the content area. For hardware-bound volumes: "This volume can only be unlocked on the Mac it came from."
- **Needs elevation:** "Reading physical disks needs administrator rights." with a "Restart as administrator" button. Disk images never need it.
- **Damaged structure:** list what could be read and add a line: "Part of this folder could not be read (catalog node 4127)."
- **Disk removed:** keep the last listing visible, dimmed, with "Samsung T7 was disconnected."

## Writing

Sentence case. Plain words: "pieces" before "extents" in the main view, "Copy to this computer" not "Extract". Technical terms are fine in the Details and Bytes views, where the user went looking for them. An action keeps one name everywhere it appears.
