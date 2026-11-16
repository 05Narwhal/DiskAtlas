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

//! The disk strip: a thin bar above the file list that draws the current device to scale.
//!
//! Every volume of the disk is a segment as wide as its share of the bytes, with a label under it;
//! space no partition covers is drawn as an outlined "Free" segment. Clicking a volume opens it,
//! like its row in the sidebar. The strip is shown only while the current location is on a disk or
//! an image, and reads only what [`Sources`] already holds: no disk is touched here.
//!
//! Each volume's bar is shaded by how full its regions are (once [`Strip`] has read the volume's
//! allocation map), and the current volume's bar carries an accent mark for each stretch of the
//! disk that the selected file occupies. Both arrive as ready-made runs; painting only maps
//! fractions of the bar to pixels.
//!
//! The segment layout is a plain function ([`layout`]) so it can be tested without a window.

use std::sync::Arc;

use gpui::{
    AnyElement, Bounds, Canvas, ClickEvent, Context, Corners, Div, Entity, FontWeight,
    HighlightStyle, Hsla, IntoElement, Pixels, Render, SharedString, StyledText, Subscription,
    Window, canvas, div, fill, point, prelude::*, px, size,
};

use crate::format;
use crate::state::navigation::{Location, Navigation};
use crate::state::sources::{Disk, SourceId, SourceKind, Sources};
use crate::state::strip::{MarkRun, ShadeRun, Strip, level_opacity, pixel_spans};
use crate::theme::{
    Colors, Theme, metrics,
    scale::{space, text},
};

/// A gap between partitions narrower than this share of the disk (1 in `GAP_DENOMINATOR`, that is
/// 0.1 percent) is not drawn: it would be a sliver nobody can see.
const GAP_DENOMINATOR: u128 = 1000;

/// What one segment of the strip stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// The volume at this position in the list given to [`layout`].
    Volume(usize),
    /// Space that no volume covers.
    Unallocated,
}

/// One stretch of the disk, from `start` for `len` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// What the stretch holds.
    pub kind: SegmentKind,
    /// Byte offset on the disk, within `0..=disk_size`.
    pub start: u64,
    /// Length in bytes. Never reaches past the end of the disk. A volume squeezed out by
    /// overlapping neighbours, or lying beyond the end of the disk, has 0 and is drawn as a
    /// sliver.
    pub len: u64,
}

/// Lays a disk of `disk_size` bytes out as segments from left to right.
///
/// `volumes` are the `(start, len)` of each volume as the partition map gave them: they may be
/// out of order, overlap or run past the end of the disk. Every volume gets exactly one segment,
/// in order of start offset (ties keep list order); a volume overlapping an earlier one starts
/// where that one ends, and nothing starts before 0 or ends after the disk. Space between one
/// segment and the next, and after the last up to the end of the disk, becomes an unallocated
/// segment unless it is under 0.1 percent of the disk. A disk of size 0 has no segments.
///
/// Arithmetic on the offsets saturates: nothing here can panic or go negative.
pub fn layout(disk_size: u64, volumes: &[(u64, u64)]) -> Vec<Segment> {
    if disk_size == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..volumes.len()).collect();
    order.sort_by_key(|&index| {
        (
            volumes.get(index).map_or(u64::MAX, |volume| volume.0),
            index,
        )
    });

    let mut segments = Vec::with_capacity(volumes.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0u64;
    for index in order {
        let Some(&(start, len)) = volumes.get(index) else {
            continue;
        };
        let start = start.min(disk_size);
        let end = start.saturating_add(len).min(disk_size);
        // Where this volume's own stretch begins once overlap with earlier ones is cut away.
        let begin = start.max(cursor);
        let end = end.max(begin);
        push_gap(&mut segments, cursor, begin, disk_size);
        segments.push(Segment {
            kind: SegmentKind::Volume(index),
            start: begin,
            len: end.saturating_sub(begin),
        });
        cursor = end;
    }
    push_gap(&mut segments, cursor, disk_size, disk_size);
    segments
}

/// Adds an unallocated segment for `from..to` when it is at least 0.1 percent of the disk.
fn push_gap(segments: &mut Vec<Segment>, from: u64, to: u64, disk_size: u64) {
    let gap = to.saturating_sub(from);
    if gap > 0 && u128::from(gap).saturating_mul(GAP_DENOMINATOR) >= u128::from(disk_size) {
        segments.push(Segment {
            kind: SegmentKind::Unallocated,
            start: from,
            len: gap,
        });
    }
}

/// One segment as the view draws it.
struct Piece {
    /// The source a click opens; `None` for unallocated space.
    target: Option<SourceId>,
    /// Bytes, which sets the segment's width.
    len: u64,
    /// The volume's name and size; `None` for unallocated space.
    label: Option<(SharedString, u64)>,
    is_current: bool,
    /// How full each region of the volume is, once known.
    shade: Option<Arc<[ShadeRun]>>,
    /// Where the selected file lives on the volume; only on the current volume.
    marks: Option<Arc<[MarkRun]>>,
    /// How many volumes share the segment's bytes (the volumes of one container); 1 for most.
    shared: usize,
}

/// The line under a segment whose bytes several volumes share: "3 volumes share this space".
pub(crate) fn shared_note(count: usize) -> Option<String> {
    (count > 1).then(|| format!("{count} volumes share this space"))
}

/// The pieces of `disk`, left to right. A disk that failed as a whole is one piece.
fn pieces(disk: &Disk, sources: &Sources, strip: &Strip, current: SourceId) -> Vec<Piece> {
    if let Some(whole) = disk.whole.and_then(|id| sources.source(id)) {
        return vec![Piece {
            target: Some(whole.id),
            len: disk.size.max(1),
            label: Some((whole.label.clone(), disk.size)),
            is_current: whole.id == current,
            shade: None,
            marks: None,
            shared: 1,
        }];
    }
    // Volumes sharing a partition (a container's) are one segment, named for the current one
    // when it is among them, else for the first in sidebar order.
    let spans = sources.spans(disk);
    // A snapshot being browsed is drawn as its volume, which lies on the same bytes.
    let live = sources.live_volume(current);
    let ranges: Vec<(u64, u64)> = spans.iter().map(|span| (span.start, span.len)).collect();
    layout(disk.size, &ranges)
        .into_iter()
        .filter_map(|segment| match segment.kind {
            SegmentKind::Volume(index) => {
                let span = spans.get(index)?;
                let id = span.lead(Some(live))?;
                let name = sources.source(id)?.label.clone();
                let is_current = span.holds(Some(live));
                Some(Piece {
                    target: Some(id),
                    len: segment.len,
                    label: Some((name, span.len)),
                    is_current,
                    // The volumes of a container share one map: whichever has loaded.
                    shade: is_current
                        .then(|| strip.shading(current).cloned())
                        .flatten()
                        .or_else(|| {
                            span.volumes
                                .iter()
                                .find_map(|&volume| strip.shading(volume).cloned())
                        }),
                    marks: if is_current {
                        strip.marks(current).cloned()
                    } else {
                        None
                    },
                    shared: span.volumes.len(),
                })
            }
            SegmentKind::Unallocated => Some(Piece {
                target: None,
                len: segment.len,
                label: None,
                is_current: false,
                shade: None,
                marks: None,
                shared: 0,
            }),
        })
        .collect()
}

/// The strip's header detail: "931.5 GB, read-only". The app never writes, so it says so.
fn header_detail(size: u64) -> String {
    if size == 0 {
        "read-only".to_owned()
    } else {
        format!("{}, read-only", format::size(size))
    }
}

/// The label of the current volume: its name, a space, then its size. Returns the text and the
/// byte length of the name, which is the part set in bold.
fn current_label(name: &str, size: u64) -> (String, usize) {
    if size == 0 {
        (name.to_owned(), name.len())
    } else {
        (format!("{name} {}", format::size(size)), name.len())
    }
}

/// The label of a volume that is not current: "Time Machine, 400 GB".
fn other_label(name: &str, size: u64) -> String {
    if size == 0 {
        name.to_owned()
    } else {
        format!("{name}, {}", format::size(size))
    }
}

/// The disk strip.
pub struct DiskStrip {
    strip: Entity<Strip>,
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    _subscriptions: Vec<Subscription>,
}

impl DiskStrip {
    /// Draws the disk of the current location from `sources`, shaded and marked with what `strip`
    /// has read, and opens the volume the user clicks through `navigation`.
    pub fn new(
        strip: Entity<Strip>,
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&strip, |_, _, cx| cx.notify()),
            cx.observe(&sources, |_, _, cx| cx.notify()),
            cx.observe(&navigation, |_, _, cx| cx.notify()),
        ];
        Self {
            strip,
            sources,
            navigation,
            _subscriptions: subscriptions,
        }
    }

    /// One segment: the bar with its label under it, as wide as its share of the disk.
    fn segment(
        &self,
        index: usize,
        piece: &Piece,
        is_first: bool,
        is_last: bool,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mut bar = div()
            .h(metrics::STRIP_BAR_HEIGHT)
            .w_full()
            .when(is_first, |bar| bar.rounded_l(metrics::STRIP_RADIUS))
            .when(is_last, |bar| bar.rounded_r(metrics::STRIP_RADIUS));
        let label = match &piece.label {
            Some((name, size)) => {
                // The current volume is at full strength and the others dimmed.
                let dim = if piece.is_current {
                    1.0
                } else {
                    metrics::STRIP_DIM_OPACITY
                };
                if piece.shade.is_some() || piece.marks.is_some() {
                    let base = match &piece.shade {
                        Some(runs) => Base::Shaded(Arc::clone(runs)),
                        // Fullness not known, but the selected file's marks are.
                        None => Base::Flat(metrics::STRIP_FILL_OPACITY),
                    };
                    bar = bar.bg(colors.track).child(
                        bar_canvas(base, piece.marks.clone(), dim, *colors, is_first, is_last)
                            .size_full(),
                    );
                } else {
                    // The fill says "fullness not known yet".
                    bar = bar
                        .bg(colors.relief)
                        .opacity(metrics::STRIP_FILL_OPACITY * dim);
                }
                if piece.is_current {
                    current_label_element(name, *size, colors)
                } else {
                    label_line(colors).child(other_label(name, *size))
                }
            }
            None => {
                // A dashed outline, no fill: space no volume covers.
                bar = bar.border_1().border_dashed().border_color(colors.line);
                label_line(colors).child("Free")
            }
        };

        // Width follows the bytes: grow by byte count from a basis of 0. `min_w` keeps a tiny or
        // squeezed-out partition a few pixels wide so it can still be seen and clicked; the
        // others share what is left.
        let mut column = div()
            .flex()
            .flex_col()
            .gap(space::S8)
            .flex_basis(px(0.))
            .min_w(metrics::STRIP_SEGMENT_MIN_WIDTH)
            .child(bar)
            .child(label)
            .children(shared_note(piece.shared).map(|note| label_line(colors).child(note)));
        // Byte counts are far larger than 1, so the factors always add up past the point where
        // flexbox would leave part of the row unfilled. Precision loss in the cast is harmless.
        column.style().flex_grow = Some(piece.len as f32);

        match piece.target {
            Some(target) => column
                .id(("disk-strip-segment", index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let to = this.sources.read(cx).source(target).map(Location::root);
                    if let Some(to) = to {
                        this.navigation
                            .update(cx, |navigation, cx| navigation.navigate(to, cx));
                    }
                }))
                .into_any_element(),
            None => column.into_any_element(),
        }
    }
}

/// What a bar is filled with before the marks go on top.
enum Base {
    /// Shaded by how full each region is.
    Shaded(Arc<[ShadeRun]>),
    /// One flat fill at this opacity.
    Flat(f32),
}

/// Paints the stretch from `x0` to `x1` pixels across the bar at `bounds`, full height. The outer
/// corners of the strip (`round` is left, right) are rounded where the stretch reaches them.
fn paint_rect(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    x0: f32,
    x1: f32,
    round: (bool, bool),
    color: Hsla,
) {
    if x1 <= x0 {
        return;
    }
    let width = f32::from(bounds.size.width);
    let radius = |on: bool| if on { metrics::STRIP_RADIUS } else { px(0.) };
    let left = radius(round.0 && x0 <= 0.0);
    let right = radius(round.1 && x1 >= width);
    let rect = Bounds::new(
        point(bounds.origin.x + px(x0), bounds.origin.y),
        size(px(x1 - x0), bounds.size.height),
    );
    let mut quad = fill(rect, color);
    quad.corner_radii = Corners {
        top_left: left,
        bottom_left: left,
        top_right: right,
        bottom_right: right,
    };
    window.paint_quad(quad);
}

/// The bar of a volume: `base` in `relief` (times `dim`), then one accent mark per stretch of the
/// selected file. `round_left` and `round_right` say which outer corners of the strip this bar
/// has. Paint does nothing heavier than turn the stored fractions into pixels.
fn bar_canvas(
    base: Base,
    marks: Option<Arc<[MarkRun]>>,
    dim: f32,
    colors: Colors,
    round_left: bool,
    round_right: bool,
) -> Canvas<()> {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let width = f32::from(bounds.size.width);
            if width <= 0.0 {
                return;
            }
            let round = (round_left, round_right);
            let origin = f32::from(bounds.origin.x);
            let scale = window.scale_factor();
            // Snap the inner edges to device pixels so neighbouring runs leave no seam.
            let edge = |fraction: f32| -> f32 {
                if fraction <= 0.0 {
                    0.0
                } else if fraction >= 1.0 {
                    width
                } else {
                    let snapped = ((origin + fraction * width) * scale).round() / scale;
                    (snapped - origin).clamp(0.0, width)
                }
            };
            match &base {
                Base::Shaded(runs) => {
                    for run in runs.iter() {
                        let color = colors.relief.opacity(level_opacity(run.level) * dim);
                        paint_rect(window, bounds, edge(run.start), edge(run.end), round, color);
                    }
                }
                Base::Flat(opacity) => {
                    let color = colors.relief.opacity(opacity * dim);
                    paint_rect(window, bounds, 0.0, width, round, color);
                }
            }
            if let Some(marks) = &marks {
                pixel_spans(
                    marks,
                    width,
                    f32::from(metrics::STRIP_MARK_MIN_WIDTH),
                    |x0, x1| paint_rect(window, bounds, x0, x1, round, colors.pin),
                );
            }
        },
    )
}

/// A single-line, ellipsized label in the secondary colour.
fn label_line(colors: &Colors) -> Div {
    div()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(text::SMALL.size)
        .text_color(colors.ink3)
}

/// "Name  size": the name in bold, the size in the secondary colour, on one ellipsized line.
fn current_label_element(name: &str, size: u64, colors: &Colors) -> Div {
    let (text, name_len) = current_label(name, size);
    let bold = HighlightStyle {
        font_weight: Some(FontWeight::BOLD),
        ..HighlightStyle::default()
    };
    let secondary = HighlightStyle {
        color: Some(colors.ink3),
        ..HighlightStyle::default()
    };
    // The single space between name and size stays in the default colour.
    let detail_start = name_len.saturating_add(1);
    let mut highlights = vec![(0..name_len, bold)];
    if detail_start < text.len() {
        highlights.push((detail_start..text.len(), secondary));
    }
    div()
        .w_full()
        .min_w_0()
        .truncate()
        .text_size(text::SMALL.size)
        .child(StyledText::new(text).with_highlights(highlights))
}

impl Render for DiskStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let sources = self.sources.read(cx);
        let strip = self.strip.read(cx);
        let current = self
            .navigation
            .read(cx)
            .current()
            .map(|location| location.source());
        let on_disk = current.and_then(|id| {
            let disk = match sources.source(id)?.kind {
                SourceKind::Volume { disk, .. } | SourceKind::WholeDisk { disk } => disk,
                SourceKind::Host => return None,
            };
            Some((id, sources.disk(disk)?))
        });
        // Host folders and no location: nothing to draw, and no space taken.
        let Some((current, disk)) = on_disk else {
            return div();
        };

        let pieces = pieces(disk, sources, strip, current);
        let last = pieces.len().saturating_sub(1);
        let segments: Vec<AnyElement> = pieces
            .iter()
            .enumerate()
            .map(|(index, piece)| {
                self.segment(index, piece, index == 0, index == last, &colors, cx)
            })
            .collect();

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(space::S8)
            .pt(space::S12)
            .px(space::S16)
            .pb(metrics::STRIP_PADDING_BOTTOM)
            .border_b_1()
            .border_color(colors.line)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(metrics::STRIP_HEADER_GAP)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::BOLD)
                            .child(disk.label.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.ink3)
                            .child(header_detail(disk.size)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(metrics::STRIP_SEGMENT_GAP)
                    .children(segments),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SegmentKind::{Unallocated, Volume};

    fn seg(kind: SegmentKind, start: u64, len: u64) -> Segment {
        Segment { kind, start, len }
    }

    #[test]
    fn only_a_shared_segment_has_a_note() {
        assert_eq!(shared_note(0), None);
        assert_eq!(shared_note(1), None);
        assert_eq!(
            shared_note(3).as_deref(),
            Some("3 volumes share this space")
        );
    }

    #[test]
    fn a_zero_size_disk_has_no_segments() {
        assert!(layout(0, &[]).is_empty());
        assert!(layout(0, &[(0, 100)]).is_empty());
    }

    #[test]
    fn a_disk_without_volumes_is_all_unallocated() {
        assert_eq!(layout(1000, &[]), vec![seg(Unallocated, 0, 1000)]);
    }

    #[test]
    fn a_volume_that_fills_the_disk_is_the_only_segment() {
        assert_eq!(layout(1000, &[(0, 1000)]), vec![seg(Volume(0), 0, 1000)]);
    }

    #[test]
    fn gaps_before_between_and_after_volumes_are_unallocated() {
        assert_eq!(
            layout(1000, &[(100, 200), (400, 100)]),
            vec![
                seg(Unallocated, 0, 100),
                seg(Volume(0), 100, 200),
                seg(Unallocated, 300, 100),
                seg(Volume(1), 400, 100),
                seg(Unallocated, 500, 500),
            ]
        );
    }

    #[test]
    fn touching_volumes_leave_no_gap() {
        assert_eq!(
            layout(300, &[(0, 100), (100, 200)]),
            vec![seg(Volume(0), 0, 100), seg(Volume(1), 100, 200)]
        );
    }

    #[test]
    fn a_gap_under_a_tenth_of_a_percent_is_ignored() {
        // 999 of 1_000_000 bytes is 0.0999 percent; 1000 is exactly 0.1 percent and is kept.
        assert_eq!(
            layout(1_000_000, &[(0, 500_000), (500_999, 499_001)]),
            vec![seg(Volume(0), 0, 500_000), seg(Volume(1), 500_999, 499_001)]
        );
        assert_eq!(
            layout(1_000_000, &[(0, 500_000), (501_000, 499_000)]),
            vec![
                seg(Volume(0), 0, 500_000),
                seg(Unallocated, 500_000, 1000),
                seg(Volume(1), 501_000, 499_000),
            ]
        );
    }

    #[test]
    fn a_small_gap_at_the_end_is_ignored_too() {
        assert_eq!(
            layout(1_000_000, &[(0, 999_500)]),
            vec![seg(Volume(0), 0, 999_500)]
        );
    }

    #[test]
    fn volumes_are_ordered_by_start_and_keep_their_index() {
        assert_eq!(
            layout(300, &[(200, 100), (0, 100)]),
            vec![
                seg(Volume(1), 0, 100),
                seg(Unallocated, 100, 100),
                seg(Volume(0), 200, 100),
            ]
        );
    }

    #[test]
    fn overlapping_volumes_are_cut_at_the_earlier_ones_end() {
        assert_eq!(
            layout(1000, &[(0, 600), (400, 600)]),
            vec![seg(Volume(0), 0, 600), seg(Volume(1), 600, 400)]
        );
    }

    #[test]
    fn a_volume_inside_another_has_zero_length_and_stays() {
        assert_eq!(
            layout(1000, &[(0, 1000), (100, 100)]),
            vec![seg(Volume(0), 0, 1000), seg(Volume(1), 1000, 0)]
        );
    }

    #[test]
    fn a_volume_past_the_end_is_clamped_to_the_disk() {
        assert_eq!(
            layout(1000, &[(800, 500), (2000, 10)]),
            vec![
                seg(Unallocated, 0, 800),
                seg(Volume(0), 800, 200),
                seg(Volume(1), 1000, 0),
            ]
        );
    }

    #[test]
    fn extreme_values_do_not_overflow() {
        let layout = layout(u64::MAX, &[(u64::MAX, u64::MAX), (1, u64::MAX)]);
        let mut at = 0u64;
        for segment in &layout {
            assert!(segment.start >= at);
            at = segment
                .start
                .checked_add(segment.len)
                .expect("a segment stays on the disk");
        }
        assert_eq!(layout.iter().filter(|s| s.kind != Unallocated).count(), 2);
    }

    #[test]
    fn segments_never_overlap_and_stay_on_the_disk() {
        let volumes = [(50, 400), (0, 100), (300, 900), (120, 0), (999, 1)];
        let size = 1000;
        let segments = layout(size, &volumes);
        let mut at = 0u64;
        for segment in &segments {
            assert!(segment.start >= at, "{segments:?}");
            at = segment.start + segment.len;
            assert!(at <= size, "{segments:?}");
        }
        for index in 0..volumes.len() {
            let count = segments.iter().filter(|s| s.kind == Volume(index)).count();
            assert_eq!(count, 1, "volume {index} appears once in {segments:?}");
        }
    }

    #[test]
    fn labels_follow_the_spec() {
        assert_eq!(header_detail(0), "read-only");
        assert_eq!(header_detail(1 << 30), "1.0 GB, read-only");
        assert_eq!(
            current_label("Macintosh HD", 1 << 30),
            ("Macintosh HD 1.0 GB".to_owned(), 12)
        );
        assert_eq!(current_label("Mac", 0), ("Mac".to_owned(), 3));
        assert_eq!(
            other_label("Time Machine", 400 << 20),
            "Time Machine, 400.0 MB"
        );
        assert_eq!(other_label("Time Machine", 0), "Time Machine");
    }
}
