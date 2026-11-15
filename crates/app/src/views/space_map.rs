// Disk Atlas - A cross-platform filesystem explorer.
//
// Copyright (C) 2026 Julian A. Loy.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
//
// See the LICENSE file for the full license text.

//! The space map (`r1-redesign.md` 3.5): the folder shown as a map of regions sized by bytes.
//!
//! Regions are folders, filled with the wash of their dominant kind and outlined by borders that
//! thin out with depth (2 px for the level shown, 1 px below). Free space is hatched water with an
//! italic name. The region of the selected file is outlined in `pin`. Clicking a folder goes in:
//! the old picture zooms until the folder fills the map (`EMPHASIZED`), then the new one takes
//! over. Backspace goes out, Enter opens the folder in the list.
//!
//! The layout is `disk_atlas_lib::sizes::layout`, pure arithmetic, made in prepaint and kept for
//! as long as the tree and the size of the map stay the same. Paint draws at most
//! `MAX_TILES` quads and the few labels that fit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use disk_atlas_lib::search::kind::KindClass;
use disk_atlas_lib::sizes::{
    LABEL_MIN_H, LABEL_MIN_W, LayoutOptions, MapNode, MapTarget, Rect, Tile, layout,
};
use gpui::{
    AnyElement, App, Bounds, Context, Corners, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, SharedString,
    Subscription, TextRun, Window, canvas, div, fill, point, prelude::*, px, size,
};

use crate::format;
use crate::keymap::{SPACE_MAP, space_map as keys};
use crate::state::space_map::{Loaded, MapState, SpaceMap};
use crate::state::view::ViewState;
use crate::theme::glyphs::Glyph;
use crate::theme::{
    Colors, Theme,
    motion::{self, EMPHASIZED},
    scale::{space, text},
};

/// Border between a folder region and what it holds, and the band at its top for its name.
const OPTIONS: LayoutOptions = LayoutOptions {
    inset: 2.0,
    header: 16.0,
};

/// Width of a region's border at the level shown and below it.
const BORDER_TOP: f32 = 2.0;
const BORDER_BELOW: f32 = 1.0;

/// Regions that do not hold the emphasised kind are drawn at this opacity.
const DIMMED: f32 = 0.35;

/// Stroke and gap of the water's hatching.
const HATCH: (f32, f32) = (1.0, 6.0);

/// What the last paint laid out, for hit tests, and for the zoom to start from.
struct Scene {
    loaded: Arc<Loaded>,
    tiles: Vec<Tile>,
    /// Size of the map the layout was made for.
    size: (f32, f32),
}

/// The old picture zooming into one of its regions.
struct Zoom {
    scene: Rc<Scene>,
    /// The region that grows to fill the map, in the scene's own coordinates.
    to: Rect,
    start: Instant,
}

/// The hovered region: its path in the tree, and the text in words.
#[derive(Debug, Clone, PartialEq)]
struct Hover {
    /// The tree the path is in; a hover over another tree is stale.
    tree: usize,
    path: (u32, Option<u32>),
    text: SharedString,
}

/// The space map view.
pub struct SpaceMapView {
    map: Entity<SpaceMap>,
    view: Entity<ViewState>,
    focus: FocusHandle,
    scene: Rc<RefCell<Option<Rc<Scene>>>>,
    /// Where the map was drawn last: the pointer is measured from here.
    origin: Rc<Cell<Point<Pixels>>>,
    hover: Option<Hover>,
    zoom: Option<Zoom>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SpaceMapView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The region of a tile, as the text and the colours need it.
struct Region {
    name: SharedString,
    bytes: u64,
    class: KindClass,
    kind: RegionKind,
    lower_bound: bool,
    /// What the region holds, drawn inside it.
    children: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    Folder,
    File,
    Free,
    Merged,
}

/// What `tile` stands for in `root`.
fn region_of(root: &MapNode, tile: &Tile) -> Option<Region> {
    if let Some((count, bytes)) = tile.merged {
        return Some(Region {
            name: format!("{count} smaller items").into(),
            bytes,
            class: KindClass::Other,
            kind: RegionKind::Merged,
            lower_bound: false,
            children: 0,
        });
    }
    let first = root.children.get(tile.path.0 as usize)?;
    let node = match tile.path.1 {
        None => first,
        Some(second) => first.children.get(second as usize)?,
    };
    let item = &node.item;
    let kind = match item.target {
        MapTarget::Free => RegionKind::Free,
        MapTarget::Merged => RegionKind::Merged,
        _ if item.is_folder => RegionKind::Folder,
        MapTarget::HostFiles(_) => RegionKind::Folder,
        _ => RegionKind::File,
    };
    let name: SharedString = match &item.target {
        MapTarget::HostFiles(_) => format!("Files in {}", item.name).into(),
        _ => item.name.clone().into(),
    };
    Some(Region {
        name,
        bytes: item.bytes,
        class: item.class,
        kind,
        lower_bound: item.lower_bound,
        children: node.children.len(),
    })
}

/// The topmost tile under `at` (relative to the map), preferring the deeper level.
fn hit(tiles: &[Tile], at: (f32, f32)) -> Option<usize> {
    tiles.iter().enumerate().rev().find_map(|(i, tile)| {
        let r = tile.rect;
        (at.0 >= r.x && at.0 < r.x + r.w && at.1 >= r.y && at.1 < r.y + r.h).then_some(i)
    })
}

/// Where a rectangle goes `t` of the way (0 to 1) of a zoom that ends with `to` filling a map of
/// `(w, h)`. Corners move in a straight line, so a rectangle stays a rectangle.
fn zoomed(r: Rect, to: Rect, map: (f32, f32), t: f32) -> Rect {
    let (sx, sy) = (map.0 / to.w.max(1.0), map.1 / to.h.max(1.0));
    let end = Rect {
        x: (r.x - to.x) * sx,
        y: (r.y - to.y) * sy,
        w: r.w * sx,
        h: r.h * sy,
    };
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Rect {
        x: mix(r.x, end.x),
        y: mix(r.y, end.y),
        w: mix(r.w, end.w),
        h: mix(r.h, end.h),
    }
}

/// "12 %", or "under 1 %" for a sliver.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0 %".to_owned();
    }
    if part > 0 && part.saturating_mul(100) < whole {
        return "under 1 %".to_owned();
    }
    format!("{} %", (part.saturating_mul(100) + whole / 2) / whole)
}

impl SpaceMapView {
    /// Draws what `map` read, and goes into regions through it.
    pub fn new(map: Entity<SpaceMap>, view: Entity<ViewState>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&map, |_, _, cx| cx.notify()),
            cx.observe(&view, |_, _, cx| cx.notify()),
        ];
        Self {
            map,
            view,
            focus: cx.focus_handle().tab_stop(true),
            scene: Rc::new(RefCell::new(None)),
            origin: Rc::new(Cell::new(Point::default())),
            hover: None,
            zoom: None,
            _subscriptions: subscriptions,
        }
    }

    fn scene(&self) -> Option<Rc<Scene>> {
        self.scene.borrow().clone()
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(scene) = self.scene() else { return };
        let origin = self.origin.get();
        let at = (
            f32::from(event.position.x - origin.x),
            f32::from(event.position.y - origin.y),
        );
        let found = hit(&scene.tiles, at).and_then(|i| {
            let tile = scene.tiles.get(i)?;
            let region = region_of(&scene.loaded.root, tile)?;
            let whole = scene.loaded.root.item.bytes;
            let bytes = region.bytes;
            let mut text = format!(
                "{}: {}",
                region.name,
                if region.lower_bound && bytes == 0 {
                    "not measured yet".to_owned()
                } else if region.lower_bound {
                    format!("at least {}", format::size(bytes))
                } else {
                    format::size(bytes)
                }
            );
            if bytes > 0 && !region.lower_bound {
                text.push_str(&format!(
                    ", {} of {}",
                    percent(bytes, whole),
                    scene.loaded.root.item.name
                ));
            }
            Some(Hover {
                tree: Arc::as_ptr(&scene.loaded) as usize,
                path: tile.path,
                text: text.into(),
            })
        });
        // A merged tile has no path of its own to go into.
        if found != self.hover {
            self.hover = found;
            cx.notify();
        }
    }

    fn path_under(&self, position: Point<Pixels>) -> Option<(u32, Option<u32>)> {
        let scene = self.scene()?;
        let origin = self.origin.get();
        let at = (
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
        );
        let tile = scene.tiles.get(hit(&scene.tiles, at)?)?;
        tile.merged.is_none().then_some(tile.path)
    }

    fn click(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(path) = self.path_under(event.position) else {
            return;
        };
        self.go_into(path, false, cx);
    }

    /// Goes into the region at `path`; the old picture zooms into it when motion is allowed.
    fn go_into(&mut self, path: (u32, Option<u32>), open_list: bool, cx: &mut Context<Self>) {
        if !self.map.read(cx).can_enter(path) {
            if open_list {
                self.map.update(cx, |map, cx| map.enter(path, true, cx));
            }
            return;
        }
        if !open_list
            && !motion::reduced(cx)
            && let Some(scene) = self.scene()
        {
            // The region that grows: the folder's own tile, or the holder of a file.
            let tile = scene
                .tiles
                .iter()
                .find(|t| t.path == (path.0, None) && t.merged.is_none());
            let target = if path.1.is_some() {
                // A folder inside a folder, or a file: the folder's own region grows.
                scene.tiles.iter().find(|t| t.path == path).or(tile)
            } else {
                tile
            };
            if let Some(target) = target {
                self.zoom = Some(Zoom {
                    scene: Rc::clone(&scene),
                    to: target.rect,
                    start: Instant::now(),
                });
            }
        }
        self.hover = None;
        self.map
            .update(cx, |map, cx| map.enter(path, open_list, cx));
    }

    fn zoom_out(&mut self, _: &keys::ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = None;
        self.map.update(cx, |map, cx| map.leave(cx));
    }

    fn open_in_list(&mut self, _: &keys::OpenInList, _: &mut Window, cx: &mut Context<Self>) {
        // The region under the pointer, else the folder itself.
        let path = self.hover.as_ref().map(|h| h.path);
        match path {
            Some(path) => self.go_into(path, true, cx),
            None => self.view.update(cx, |view, cx| {
                view.set_mode(crate::state::view::ViewMode::List, cx);
            }),
        }
    }

    /// The text and the button when there is no map to draw.
    fn message(&self, state: &MapState, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let (title, detail, action): (SharedString, Option<SharedString>, Option<AnyElement>) =
            match state {
                MapState::Idle | MapState::Loading => (
                    "Reading sizes…".into(),
                    Some("The map appears as soon as the folders have been measured.".into()),
                    None,
                ),
                MapState::NeedsIndex(source) => {
                    let source = *source;
                    (
                        "This volume has not been measured".into(),
                        Some("The map needs the list of every file on the volume.".into()),
                        Some(
                            div()
                                .id("space-map-read")
                                .px(space::S12)
                                .py(space::S4)
                                .rounded(crate::theme::scale::radius::ROW)
                                .bg(colors.ink)
                                .text_color(colors.sheet)
                                .cursor_pointer()
                                .child("Read the file list")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.map
                                        .update(cx, |map, cx| map.read_file_list(source, cx));
                                }))
                                .into_any_element(),
                        ),
                    )
                }
                MapState::Unavailable(why) => ("There is no map".into(), Some(why.clone()), None),
                MapState::Ready => ("".into(), None, None),
            };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(space::S8)
            .child(div().text_color(colors.ink).child(title))
            .children(detail.map(|d| {
                div()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink3)
                    .child(d)
            }))
            .children(action)
            .into_any_element()
    }
}

impl Render for SpaceMapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        let colors = theme.colors;
        let (state, loaded, selected) = {
            let map = self.map.read(cx);
            (
                map.state().clone(),
                map.loaded().cloned(),
                map.selected_name().cloned(),
            )
        };
        let highlight = self.view.read(cx).highlight();
        // A hover over a tree that has been replaced (a folder was entered) is gone.
        if let (Some(hover), Some(loaded)) = (&self.hover, &loaded)
            && hover.tree != Arc::as_ptr(loaded) as usize
        {
            self.hover = None;
        }

        // The zoom ends when its time is up and the folder it went into has arrived.
        let progress = self.zoom.as_ref().map(|zoom| {
            (zoom.start.elapsed().as_secs_f32() / EMPHASIZED.duration.as_secs_f32()).min(1.0)
        });
        if let (Some(zoom), Some(p), Some(loaded)) = (&self.zoom, progress, &loaded)
            && p >= 1.0
            && !Arc::ptr_eq(&zoom.scene.loaded, loaded)
            && state == MapState::Ready
        {
            self.zoom = None;
        }
        if progress.is_some_and(|p| p < 1.0) {
            window.request_animation_frame();
        }
        let eased = progress.map(|p| EMPHASIZED.easing.apply(p));

        let body: AnyElement = match (&state, &loaded, &self.zoom) {
            (_, _, Some(zoom)) => {
                let scene = Rc::clone(&zoom.scene);
                let to = zoom.to;
                let t = eased.unwrap_or(1.0);
                self.canvas(
                    scene.loaded.clone(),
                    Some((scene, to, t)),
                    None,
                    selected,
                    highlight,
                    colors,
                )
            }
            (MapState::Ready, Some(loaded), None) => self.canvas(
                Arc::clone(loaded),
                None,
                self.hover.as_ref().map(|h| h.path),
                selected,
                highlight,
                colors,
            ),
            _ => self.message(&state, &colors, cx),
        };

        let info = self.hover.as_ref().map(|hover| {
            div()
                .absolute()
                .left(space::S12)
                .bottom(space::S12)
                .max_w_full()
                .px(space::S8)
                .py(space::S4)
                .rounded(crate::theme::scale::radius::ROW)
                .bg(colors.raised)
                .border_1()
                .border_color(colors.line)
                .text_size(text::SMALL.size)
                .text_color(colors.ink)
                .child(hover.text.clone())
        });

        div()
            .id("space-map")
            .key_context(SPACE_MAP)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::open_in_list))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(colors.sheet)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.mouse_move(event, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.click(event, window, cx);
                }),
            )
            .child(body)
            .children(info)
    }
}

impl SpaceMapView {
    /// The map's canvas. `zoom` is the scene being zoomed, the region it grows to and how far it
    /// has got; otherwise the layout is made for the canvas's own size.
    fn canvas(
        &self,
        loaded: Arc<Loaded>,
        zoom: Option<(Rc<Scene>, Rect, f32)>,
        hover: Option<(u32, Option<u32>)>,
        selected: Option<SharedString>,
        highlight: Option<KindClass>,
        colors: Colors,
    ) -> AnyElement {
        let store = Rc::clone(&self.scene);
        let origin = Rc::clone(&self.origin);
        let zooming = zoom.is_some();
        canvas(
            move |bounds, _, _| {
                origin.set(bounds.origin);
                let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                if zooming {
                    return None;
                }
                let reuse = store.borrow().as_ref().is_some_and(|scene| {
                    Arc::ptr_eq(&scene.loaded, &loaded) && scene.size == (w, h)
                });
                if !reuse {
                    let tiles = layout(
                        &loaded.root,
                        Rect {
                            x: 0.0,
                            y: 0.0,
                            w,
                            h,
                        },
                        OPTIONS,
                    );
                    *store.borrow_mut() = Some(Rc::new(Scene {
                        loaded: Arc::clone(&loaded),
                        tiles,
                        size: (w, h),
                    }));
                }
                store.borrow().clone()
            },
            move |bounds, prepared, window, cx| {
                let (scene, to, t) = match &zoom {
                    Some((scene, to, t)) => (Rc::clone(scene), Some(*to), *t),
                    None => match prepared {
                        Some(scene) => (scene, None, 0.0),
                        None => return,
                    },
                };
                let map = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    paint_scene(
                        window,
                        cx,
                        bounds,
                        &scene,
                        to.map(|to| (to, t)),
                        map,
                        hover,
                        selected.as_ref().map(|s| s.as_ref()),
                        highlight,
                        &colors,
                    );
                });
            },
        )
        .size_full()
        .into_any_element()
    }
}

/// A region's colors: its wash, and the tint its borders and names take.
fn tints(class: KindClass, colors: &Colors) -> (Hsla, Hsla) {
    let glyph = Glyph::from_class(class);
    (colors.wash(glyph), colors.tint(glyph))
}

#[allow(clippy::too_many_arguments)]
fn paint_scene(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    scene: &Scene,
    zoom: Option<(Rect, f32)>,
    map: (f32, f32),
    hover: Option<(u32, Option<u32>)>,
    selected: Option<&str>,
    highlight: Option<KindClass>,
    colors: &Colors,
) {
    let root = &scene.loaded.root;
    // While zooming, what is outside the growing region fades away.
    let fade = zoom.map_or(1.0, |(_, t)| 1.0 - t);
    for tile in &scene.tiles {
        let Some(region) = region_of(root, tile) else {
            continue;
        };
        let rect = match zoom {
            Some((to, t)) => zoomed(tile.rect, to, map, t),
            None => tile.rect,
        };
        let inside_target = zoom.is_none_or(|(to, _)| {
            tile.rect.x >= to.x - 0.5
                && tile.rect.y >= to.y - 0.5
                && tile.rect.x + tile.rect.w <= to.x + to.w + 0.5
                && tile.rect.y + tile.rect.h <= to.y + to.h + 0.5
        });
        let mut alpha = if inside_target { 1.0 } else { fade };
        if let Some(kind) = highlight
            && region.kind != RegionKind::Free
            && region.class != kind
        {
            alpha *= DIMMED;
        }
        if alpha <= 0.01 || rect.w < 1.0 || rect.h < 1.0 {
            continue;
        }
        let border = if tile.depth == 0 {
            BORDER_TOP
        } else {
            BORDER_BELOW
        };
        // Neighbours share an edge, so each draws half of it.
        let half = border / 2.0;
        let quad_bounds = Bounds::new(
            point(
                bounds.origin.x + px(rect.x + half),
                bounds.origin.y + px(rect.y + half),
            ),
            size(
                px((rect.w - border).max(0.0)),
                px((rect.h - border).max(0.0)),
            ),
        );
        let (wash, tint) = tints(region.class, colors);
        let mut quad = match region.kind {
            RegionKind::Free => fill(quad_bounds, colors.water.opacity(alpha)),
            RegionKind::Merged => fill(quad_bounds, colors.track.opacity(alpha)),
            _ => fill(quad_bounds, wash.opacity(alpha)),
        };
        quad.border_widths = gpui::Edges::all(px(border));
        quad.border_color = match region.kind {
            RegionKind::Free => colors.water_line,
            RegionKind::Merged => colors.line,
            _ => tint.opacity(if tile.depth == 0 { 0.7 } else { 0.4 }),
        }
        .opacity(alpha);
        quad.corner_radii = Corners::all(px(if tile.depth == 0 { 3.0 } else { 2.0 }));
        window.paint_quad(quad);
        if region.kind == RegionKind::Free {
            let (stroke, gap) = HATCH;
            let mut hatch = fill(
                quad_bounds,
                gpui::pattern_slash(colors.water_line.opacity(alpha), stroke, gap),
            );
            hatch.corner_radii = Corners::all(px(3.0));
            window.paint_quad(hatch);
        }
        let on_selected = tile.depth == 0
            && tile.merged.is_none()
            && selected.is_some_and(|name| name == region.name.as_ref());
        let hovered = hover == Some(tile.path) && tile.merged.is_none();
        if on_selected || hovered {
            let mut ring = fill(quad_bounds, gpui::transparent_black());
            ring.border_widths = gpui::Edges::all(px(2.0));
            ring.border_color = if on_selected { colors.pin } else { colors.ink2 }.opacity(alpha);
            ring.corner_radii = Corners::all(px(3.0));
            window.paint_quad(ring);
        }
        // A folder region too short for its header band has no room for a name: its
        // children cover it and carry their own.
        let room = tile.depth == 1
            || region.kind != RegionKind::Folder
            || region.children == 0
            || tile.rect.h >= LABEL_MIN_H + OPTIONS.header;
        if tile.labelled && room && alpha > 0.5 && zoom.is_none() {
            paint_label(window, cx, quad_bounds, tile, &region, tint, colors);
        }
    }
}

/// The region's name (and, when it fits, its size) at the top left.
fn paint_label(
    window: &mut Window,
    cx: &mut App,
    rect: Bounds<Pixels>,
    tile: &Tile,
    region: &Region,
    tint: Hsla,
    colors: &Colors,
) {
    debug_assert!(
        rect.size.width >= px(LABEL_MIN_W - 4.0)
            || rect.size.height >= px(LABEL_MIN_H - 4.0)
            || tile.labelled
    );
    let style = window.text_style();
    let font_size = text::MAP_LABEL.size;
    let pad = 5.0;
    let avail = f32::from(rect.size.width) - 2.0 * pad;
    if avail < 12.0 {
        return;
    }
    let mut font = style.font();
    font.weight = FontWeight::BOLD;
    if region.kind == RegionKind::Free {
        font.style = gpui::FontStyle::Italic;
        font.weight = FontWeight::NORMAL;
    }
    let color = match region.kind {
        RegionKind::Free | RegionKind::Merged => colors.ink2,
        _ => colors.ink,
    };
    let origin = point(rect.origin.x + px(pad), rect.origin.y + px(3.0));
    let line_height = font_size * 1.3;

    let shape = |window: &mut Window, text: String, font: &gpui::Font, color: Hsla| {
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(SharedString::from(text), font_size, &[run], None)
    };

    let mut name = region.name.to_string();
    let mut line = shape(window, name.clone(), &font, color);
    if f32::from(line.width) > avail {
        let cut = line.closest_index_for_x(px(avail) - font_size);
        let mut end = cut.min(name.len());
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name.push('…');
        line = shape(window, name.clone(), &font, color);
    }
    let name_width = f32::from(line.width);
    line.paint(origin, line_height, window, cx).ok();

    // The size: after the name on the same line when it fits, else under it in a tall region.
    let detail = match region.kind {
        RegionKind::Merged => None,
        _ if region.lower_bound && region.bytes == 0 => None,
        _ if region.lower_bound => Some(format!("at least {}", format::size(region.bytes))),
        _ => Some(format::size(region.bytes)),
    };
    let Some(detail) = detail else { return };
    let mut plain = style.font();
    plain.weight = FontWeight::NORMAL;
    let detail_color = if region.kind == RegionKind::Folder {
        tint
    } else {
        colors.ink2
    };
    let size_line = shape(window, detail, &plain, detail_color);
    let detail_width = f32::from(size_line.width);
    if name_width + 8.0 + detail_width <= avail && tile.depth == 0 {
        let at = point(origin.x + px(name_width + 8.0), origin.y);
        size_line.paint(at, line_height, window, cx).ok();
    } else if tile.depth == 1
        && f32::from(rect.size.height) >= 2.0 * f32::from(line_height) + 8.0
        && detail_width <= avail
    {
        let at = point(origin.x, origin.y + line_height);
        size_line.paint(at, line_height, window, cx).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(x: f32, y: f32, w: f32, h: f32, depth: u8, path: (u32, Option<u32>)) -> Tile {
        Tile {
            rect: Rect { x, y, w, h },
            depth,
            path,
            labelled: false,
            merged: None,
        }
    }

    #[test]
    fn a_click_goes_to_the_deeper_region_first() {
        let tiles = [
            tile(0.0, 0.0, 100.0, 100.0, 0, (0, None)),
            tile(10.0, 10.0, 50.0, 50.0, 1, (0, Some(0))),
            tile(100.0, 0.0, 100.0, 100.0, 0, (1, None)),
        ];
        assert_eq!(hit(&tiles, (20.0, 20.0)), Some(1));
        assert_eq!(hit(&tiles, (80.0, 80.0)), Some(0));
        assert_eq!(hit(&tiles, (150.0, 50.0)), Some(2));
        assert_eq!(hit(&tiles, (250.0, 50.0)), None);
        // The right and bottom edges belong to the neighbour.
        assert_eq!(hit(&tiles, (100.0, 50.0)), Some(2));
    }

    #[test]
    fn a_zoom_starts_at_the_tile_and_ends_filling_the_map() {
        let to = Rect {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 100.0,
        };
        let map = (400.0, 200.0);
        let start = zoomed(to, to, map, 0.0);
        assert_eq!(start, to);
        let end = zoomed(to, to, map, 1.0);
        assert_eq!(
            end,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 200.0
            }
        );
        let half = zoomed(to, to, map, 0.5);
        assert_eq!(half.w, 300.0);
        // A degenerate target does not divide by zero.
        let flat = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        let r = zoomed(to, flat, map, 1.0);
        assert!(r.x.is_finite() && r.w.is_finite());
    }

    #[test]
    fn percent_is_honest_about_slivers() {
        assert_eq!(percent(38, 100), "38 %");
        assert_eq!(percent(1, 10_000), "under 1 %");
        assert_eq!(percent(0, 0), "0 %");
    }
}
