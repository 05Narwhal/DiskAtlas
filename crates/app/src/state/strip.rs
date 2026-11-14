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

//! What the disk strip draws besides its layout: how full each volume is, and where the selected
//! file lives.
//!
//! Both come from the engine and both are slow, so they are fetched on the background executor
//! and turned there into draw runs (fractions of the volume's length), which is all the view maps
//! to pixels.
//!
//! - **Shading.** The first time a browsable volume becomes current, its allocation map is read
//!   once ([`load_shading`]) and the runs are cached per source for the session, a failure
//!   included ([`MapCache`]).
//! - **Marks.** While exactly one entry is selected on a volume, its data and resource extents
//!   are read ([`load_marks`]). Each request carries a generation ([`MarkTarget`]); a result for a
//!   selection the user has left is dropped.
//!
//! The conversions are plain functions so they can be tested without a window.

use std::collections::HashMap;
use std::sync::Arc;

use disk_atlas_lib::fs::{AllocationMap, Extent, FileSystem, NodeId, Stream};
use gpui::{Context, Entity, Subscription, Task};

use super::listing::Listing;
use super::navigation::Navigation;
use super::sources::{SourceId, SourceKind, Sources};
use crate::theme::metrics;

/// A stretch of a volume's bar drawn at one opacity step. Positions are fractions of the volume's
/// length, `0.0..=1.0`, with `start < end`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadeRun {
    /// Where the run begins.
    pub start: f32,
    /// Where the run ends.
    pub end: f32,
    /// The opacity step, `0..STRIP_SHADE_LEVELS`; see [`level_opacity`].
    pub level: u8,
}

/// A stretch of a volume's bar that holds part of the selected file. Positions are fractions of
/// the volume's length, `0.0..=1.0`, with `start < end`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkRun {
    /// Where the run begins.
    pub start: f32,
    /// Where the run ends.
    pub end: f32,
}

/// The opacity step of a density: 0 for an empty slice, `STRIP_SHADE_LEVELS - 1` for a full one,
/// and at least 1 for anything in between, so a slice with any used space never looks empty.
pub fn level_of(density: u8) -> u8 {
    let top = u16::from(metrics::STRIP_SHADE_LEVELS.saturating_sub(1));
    let level = (u16::from(density) * top).div_ceil(255);
    u8::try_from(level.min(top)).unwrap_or(u8::MAX)
}

/// The opacity of a step: the 6 percent floor at step 0 up to 100 percent at the last step.
pub fn level_opacity(level: u8) -> f32 {
    let top = metrics::STRIP_SHADE_LEVELS.saturating_sub(1).max(1);
    let level = level.min(top);
    metrics::STRIP_SHADE_FLOOR
        + (1.0 - metrics::STRIP_SHADE_FLOOR) * f32::from(level) / f32::from(top)
}

/// `position` as a fraction of `total`, clamped to `0.0..=1.0`.
fn fraction(position: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (position as f64 / total as f64).clamp(0.0, 1.0) as f32
}

/// Turns an allocation map into shaded runs over a volume of `volume_len` bytes.
///
/// The map covers `map.start .. map.start + map.len` of the volume in equal slices; bytes of the
/// volume outside that range are drawn at the floor (step 0). Neighbouring slices of one step
/// become one run. The runs cover `0..volume_len` without gaps. An empty map, or a volume of
/// length 0, gives no runs.
pub fn density_to_runs(map: &AllocationMap, volume_len: u64) -> Vec<ShadeRun> {
    let slices = map.density.len();
    if slices == 0 || map.len == 0 || volume_len == 0 {
        return Vec::new();
    }
    // Byte position of the boundary before slice `i`, clamped to the volume.
    let edge = |i: usize| -> u64 {
        let offset = u128::from(map.len) * (i as u128) / (slices as u128);
        let at = u128::from(map.start) + offset;
        u64::try_from(at.min(u128::from(volume_len))).unwrap_or(volume_len)
    };
    fn push(spans: &mut Vec<(u64, u64, u8)>, from: u64, to: u64, level: u8) {
        if to <= from {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.2 == level
            && last.1 == from
        {
            last.1 = to;
        } else {
            spans.push((from, to, level));
        }
    }

    let mut spans: Vec<(u64, u64, u8)> = Vec::new();
    push(&mut spans, 0, edge(0), 0);
    for (index, &density) in map.density.iter().enumerate() {
        push(
            &mut spans,
            edge(index),
            edge(index.saturating_add(1)),
            level_of(density),
        );
    }
    push(&mut spans, edge(slices), volume_len, 0);

    spans
        .into_iter()
        .map(|(from, to, level)| ShadeRun {
            start: fraction(from, volume_len),
            end: fraction(to, volume_len),
            level,
        })
        .collect()
}

/// Turns the extents of a file into marks over a volume of `volume_len` bytes.
///
/// Extents are clamped to the volume (one entirely past its end is dropped), sorted, and merged
/// when they overlap or lie less than 1/4096 of the volume apart, so the result has fewer than
/// 4097 runs however fragmented the file is. The runs are sorted and do not overlap.
pub fn extents_to_marks(extents: &[Extent], volume_len: u64) -> Vec<MarkRun> {
    if volume_len == 0 {
        return Vec::new();
    }
    let mut spans: Vec<(u64, u64)> = extents
        .iter()
        .filter_map(|extent| {
            let start = extent.offset.min(volume_len);
            let end = extent.offset.saturating_add(extent.len).min(volume_len);
            (end > start).then_some((start, end))
        })
        .collect();
    spans.sort_unstable();

    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in spans {
        if let Some(last) = merged.last_mut()
            && u128::from(start.saturating_sub(last.1))
                * u128::from(metrics::STRIP_MARK_MERGE_DENOMINATOR)
                < u128::from(volume_len)
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
        .into_iter()
        .map(|(start, end)| MarkRun {
            start: fraction(start, volume_len),
            end: fraction(end, volume_len),
        })
        .collect()
}

/// Maps `marks` (sorted by start) onto a bar `width` pixels wide and calls `paint(x0, x1)` for
/// each stretch to draw, left to right. Every stretch is at least `min_width` wide (shifted left
/// at the bar's right end, and the whole bar when it is narrower than that), and stretches that
/// overlap after that are one call. Nothing is called for a bar of no width. Allocates nothing.
pub fn pixel_spans(marks: &[MarkRun], width: f32, min_width: f32, mut paint: impl FnMut(f32, f32)) {
    if width.is_nan() || width <= 0.0 {
        return;
    }
    let mut current: Option<(f32, f32)> = None;
    for mark in marks {
        let mut x0 = (mark.start * width).clamp(0.0, width);
        let x1 = (mark.end * width).max(x0 + min_width).min(width);
        if x1 - x0 < min_width {
            x0 = (x1 - min_width).max(0.0);
        }
        current = match current {
            Some((c0, c1)) if x0 <= c1 => Some((c0.min(x0), c1.max(x1))),
            Some((c0, c1)) => {
                paint(c0, c1);
                Some((x0, x1))
            }
            None => Some((x0, x1)),
        };
    }
    if let Some((c0, c1)) = current {
        paint(c0, c1);
    }
}

/// Where one volume's allocation map stands.
#[derive(Debug, Clone, PartialEq)]
pub enum MapState {
    /// Being read on the background executor.
    InFlight,
    /// Read; these are the draw runs.
    Ready(Arc<[ShadeRun]>),
    /// Not available (unsupported, failed or empty). It is not asked for again.
    Failed,
}

/// The allocation map of every volume asked for, kept for the session.
#[derive(Debug, Default)]
pub struct MapCache {
    entries: HashMap<SourceId, MapState>,
}

impl MapCache {
    /// Notes that the map of `source` is wanted. True exactly once per source: the caller should
    /// start reading it. False when it is in flight, done or failed.
    pub fn request(&mut self, source: SourceId) -> bool {
        if self.entries.contains_key(&source) {
            return false;
        }
        self.entries.insert(source, MapState::InFlight);
        true
    }

    /// Records the outcome of a read: `None` for a failure. A source never requested is ignored.
    pub fn finish(&mut self, source: SourceId, runs: Option<Arc<[ShadeRun]>>) {
        if let Some(entry) = self.entries.get_mut(&source) {
            *entry = runs.map_or(MapState::Failed, MapState::Ready);
        }
    }

    /// Where the map of `source` stands; `None` if it was never requested.
    #[cfg(test)]
    pub fn state(&self, source: SourceId) -> Option<&MapState> {
        self.entries.get(&source)
    }

    /// The draw runs of `source`, once they have arrived.
    pub fn runs(&self, source: SourceId) -> Option<&Arc<[ShadeRun]>> {
        match self.entries.get(&source) {
            Some(MapState::Ready(runs)) => Some(runs),
            _ => None,
        }
    }
}

/// What a change of target asks of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retarget {
    /// The target did not change.
    Same,
    /// There is no target any more; the marks were cleared.
    Idle,
    /// A new target; the marks were cleared. Read its extents and hand the result back with this
    /// generation.
    Fetch(u64),
}

/// The file whose extents are marked, and the generation that tells fresh results from stale ones.
#[derive(Debug, Default)]
pub struct MarkTarget {
    key: Option<(SourceId, NodeId)>,
    generation: u64,
    runs: Arc<[MarkRun]>,
}

impl MarkTarget {
    /// Aims at `key` (the volume and node of the one selected entry), or at nothing. Changing the
    /// target bumps the generation and clears the marks.
    pub fn retarget(&mut self, key: Option<(SourceId, NodeId)>) -> Retarget {
        if key == self.key {
            return Retarget::Same;
        }
        self.key = key;
        self.generation = self.generation.wrapping_add(1);
        self.runs = Arc::from([]);
        match key {
            Some(_) => Retarget::Fetch(self.generation),
            None => Retarget::Idle,
        }
    }

    /// Takes the marks of a finished read. True if `generation` is the current one and the marks
    /// were kept; a stale result is dropped and false.
    pub fn accept(&mut self, generation: u64, runs: Vec<MarkRun>) -> bool {
        if generation != self.generation || self.key.is_none() {
            return false;
        }
        self.runs = Arc::from(runs);
        true
    }

    /// The marks to draw on `source`'s bar: none unless they belong to that volume.
    pub fn runs_for(&self, source: SourceId) -> Option<&Arc<[MarkRun]>> {
        match self.key {
            Some((id, _)) if id == source && !self.runs.is_empty() => Some(&self.runs),
            _ => None,
        }
    }
}

/// Reads the allocation map of `fs` and turns it into draw runs for a volume of `volume_len`
/// bytes. `None` when the filesystem cannot say, or the map is empty. Blocking: it reads the
/// volume's whole bitmap.
pub fn load_shading(fs: &dyn FileSystem, volume_len: u64) -> Option<Arc<[ShadeRun]>> {
    match fs.allocation_map(metrics::STRIP_MAP_BUCKETS) {
        Ok(map) => {
            let runs = density_to_runs(&map, volume_len);
            (!runs.is_empty()).then(|| Arc::from(runs))
        }
        Err(error) => {
            tracing::debug!(%error, "no allocation map for the disk strip");
            None
        }
    }
}

/// Reads the data and resource extents of `node` and turns them into marks for a volume of
/// `volume_len` bytes. A stream whose extents cannot be read adds no marks; the other stream's
/// still show, so a damaged resource fork does not hide where the data is. Blocking.
pub fn load_marks(fs: &dyn FileSystem, node: NodeId, volume_len: u64) -> Vec<MarkRun> {
    let mut extents = Vec::new();
    for stream in [Stream::Data, Stream::Resource] {
        match fs.extents(node, stream) {
            Ok(mut found) => extents.append(&mut found),
            Err(error) => {
                tracing::debug!(%error, ?stream, "no extents for the disk strip");
            }
        }
    }
    extents_to_marks(&extents, volume_len)
}

/// The disk strip's data: the shading of every volume seen and the marks of the selected file.
pub struct Strip {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    listing: Entity<Listing>,
    maps: MapCache,
    marks: MarkTarget,
    marks_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Strip {
    /// Follows `navigation` and `listing`: reads the allocation map of each volume that becomes
    /// current, and the extents of the one selected entry.
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        listing: Entity<Listing>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&navigation, |this, _, cx| this.refresh(cx)),
            cx.observe(&listing, |this, _, cx| this.refresh(cx)),
        ];
        let mut strip = Self {
            sources,
            navigation,
            listing,
            maps: MapCache::default(),
            marks: MarkTarget::default(),
            marks_task: None,
            _subscriptions: subscriptions,
        };
        strip.refresh(cx);
        strip
    }

    /// The draw runs of `source`'s allocation map, once read.
    pub fn shading(&self, source: SourceId) -> Option<&Arc<[ShadeRun]>> {
        self.maps.runs(source)
    }

    /// The marks of the selected file on `source`'s bar.
    pub fn marks(&self, source: SourceId) -> Option<&Arc<[MarkRun]>> {
        self.marks.runs_for(source)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request_map(cx);
        self.retarget_marks(cx);
    }

    /// Starts reading the allocation map of the current source if it is a browsable volume whose
    /// map has not been asked for.
    fn request_map(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .navigation
            .read(cx)
            .current()
            .map(|location| location.source())
        else {
            return;
        };
        let Some((fs, len)) = self.sources.read(cx).source(id).and_then(|source| {
            let SourceKind::Volume { len, .. } = source.kind else {
                return None;
            };
            source
                .availability
                .is_browsable()
                .then(|| (Arc::clone(&source.fs), len))
        }) else {
            return;
        };
        if !self.maps.request(id) {
            return;
        }
        // Detached on purpose: a map that arrives after the user moved on is still cached.
        cx.spawn(async move |this, cx| {
            let runs = cx
                .background_executor()
                .spawn(async move { load_shading(fs.as_ref(), len) })
                .await;
            this.update(cx, |this, cx| {
                this.maps.finish(id, runs);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Aims the marks at the one selected entry of the listing, if there is exactly one and the
    /// listing shows a browsable volume.
    fn retarget_marks(&mut self, cx: &mut Context<Self>) {
        let listing = self.listing.read(cx);
        let key = listing.location().and_then(|location| {
            if listing.selected_count() != 1 {
                return None;
            }
            let entry = listing.primary()?;
            let source = self.sources.read(cx).source(location.source())?;
            let is_volume = matches!(source.kind, SourceKind::Volume { .. });
            (is_volume && source.availability.is_browsable()).then_some((source.id, entry.node))
        });

        match self.marks.retarget(key) {
            Retarget::Same => {}
            Retarget::Idle => {
                self.marks_task = None;
                cx.notify();
            }
            Retarget::Fetch(generation) => {
                cx.notify();
                self.marks_task = key.and_then(|(id, node)| {
                    let source = self.sources.read(cx).source(id)?;
                    let SourceKind::Volume { len, .. } = source.kind else {
                        return None;
                    };
                    let fs = Arc::clone(&source.fs);
                    Some(cx.spawn(async move |this, cx| {
                        let runs = cx
                            .background_executor()
                            .spawn(async move { load_marks(fs.as_ref(), node, len) })
                            .await;
                        this.update(cx, |this, cx| {
                            if this.marks.accept(generation, runs) {
                                cx.notify();
                            }
                        })
                        .ok();
                    }))
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::error::{Error, Result as EngineResult};
    use disk_atlas_lib::fs::{DirStream, Metadata, VolumeInfo};

    fn map(start: u64, len: u64, density: &[u8]) -> AllocationMap {
        AllocationMap::new(start, len, density.to_vec())
    }

    fn run(start: f32, end: f32, level: u8) -> ShadeRun {
        ShadeRun { start, end, level }
    }

    fn mark(start: f32, end: f32) -> MarkRun {
        MarkRun { start, end }
    }

    fn extent(offset: u64, len: u64) -> Extent {
        Extent { offset, len }
    }

    const TOP: u8 = 15;

    #[test]
    fn the_floor_and_the_full_value_are_6_and_100_percent() {
        assert_eq!(level_of(0), 0);
        assert_eq!(level_of(255), TOP);
        assert!((level_opacity(0) - 0.06).abs() < 1e-6);
        assert!((level_opacity(TOP) - 1.0).abs() < 1e-6);
        // Past the top step it stays at 100 percent.
        assert!((level_opacity(200) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn any_used_space_is_above_the_floor() {
        assert_eq!(level_of(1), 1);
        assert!(level_opacity(level_of(1)) > level_opacity(0));
    }

    #[test]
    fn densities_quantize_to_sixteen_steps() {
        let levels: std::collections::BTreeSet<u8> = (0..=255u8).map(level_of).collect();
        assert_eq!(levels.len(), 16);
        assert!(levels.iter().copied().eq(0..=TOP));
        // Monotone.
        assert!((1..=255u8).all(|d| level_of(d) >= level_of(d - 1)));
    }

    #[test]
    fn equal_neighbours_merge_into_one_run() {
        let runs = density_to_runs(&map(0, 400, &[255, 255, 0, 0]), 400);
        assert_eq!(runs, vec![run(0.0, 0.5, TOP), run(0.5, 1.0, 0)]);
    }

    #[test]
    fn densities_that_quantize_alike_merge() {
        // 254 and 255 are both the top step.
        let runs = density_to_runs(&map(0, 300, &[254, 255, 128]), 300);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs.first().map(|r| r.level), Some(TOP));
        assert_eq!(runs.first().map(|r| r.end), Some(fraction(200, 300)));
    }

    #[test]
    fn a_map_that_starts_after_the_partition_start_is_floored_before_it() {
        // The volume is 1000 bytes; the map covers 200..600 (a wrapper offset).
        let runs = density_to_runs(&map(200, 400, &[255, 255]), 1000);
        assert_eq!(
            runs,
            vec![run(0.0, 0.2, 0), run(0.2, 0.6, TOP), run(0.6, 1.0, 0)]
        );
    }

    #[test]
    fn a_map_shorter_than_the_partition_is_floored_after_it() {
        let runs = density_to_runs(&map(0, 500, &[255, 0]), 1000);
        assert_eq!(runs, vec![run(0.0, 0.25, TOP), run(0.25, 1.0, 0)]);
    }

    #[test]
    fn runs_cover_the_volume_without_gaps() {
        let runs = density_to_runs(&map(100, 700, &[1, 90, 255, 0, 3, 3, 200]), 1000);
        assert_eq!(runs.first().map(|r| r.start), Some(0.0));
        assert_eq!(runs.last().map(|r| r.end), Some(1.0));
        for pair in runs.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
            assert_ne!(pair[0].level, pair[1].level);
        }
    }

    #[test]
    fn a_map_past_the_volume_is_clamped() {
        let runs = density_to_runs(&map(900, 1000, &[255, 255]), 1000);
        assert_eq!(runs, vec![run(0.0, 0.9, 0), run(0.9, 1.0, TOP)]);
        // Entirely past the end: only the floor.
        let runs = density_to_runs(&map(5000, 1000, &[255]), 1000);
        assert_eq!(runs, vec![run(0.0, 1.0, 0)]);
    }

    #[test]
    fn nothing_to_draw_gives_no_runs() {
        assert!(density_to_runs(&map(0, 100, &[]), 100).is_empty());
        assert!(density_to_runs(&map(0, 0, &[255]), 100).is_empty());
        assert!(density_to_runs(&map(0, 100, &[255]), 0).is_empty());
    }

    #[test]
    fn extreme_maps_do_not_overflow() {
        let runs = density_to_runs(&map(u64::MAX, u64::MAX, &[255; 512]), u64::MAX);
        assert!(!runs.is_empty());
        let runs = density_to_runs(&map(u64::MAX - 1, u64::MAX, &[7, 8, 9]), u64::MAX);
        assert_eq!(runs.last().map(|r| r.end), Some(1.0));
    }

    #[test]
    fn marks_are_sorted_fractions() {
        let marks = extents_to_marks(&[extent(800, 100), extent(100, 100)], 1_000_000);
        assert_eq!(marks.len(), 2);
        assert!(marks[0].start < marks[1].start);
        assert_eq!(marks[0].start, fraction(100, 1_000_000));
        assert_eq!(marks[1].end, fraction(900, 1_000_000));
    }

    #[test]
    fn marks_are_clamped_to_the_volume() {
        let marks = extents_to_marks(&[extent(900, 500)], 1000);
        assert_eq!(marks, vec![mark(0.9, 1.0)]);
        // Entirely past the end, and an overflowing extent.
        assert!(extents_to_marks(&[extent(1000, 10), extent(2000, 10)], 1000).is_empty());
        let marks = extents_to_marks(&[extent(10, u64::MAX)], 1000);
        assert_eq!(marks, vec![mark(0.01, 1.0)]);
    }

    #[test]
    fn extents_at_both_ends_of_the_volume() {
        let marks = extents_to_marks(&[extent(0, 100), extent(900, 100)], 1000);
        assert_eq!(marks, vec![mark(0.0, 0.1), mark(0.9, 1.0)]);
    }

    #[test]
    fn extents_merge_when_closer_than_one_4096th_of_the_volume() {
        let volume = 4096 * 100;
        // Gap of 99 bytes merges; a gap of 100 does not.
        let merged = extents_to_marks(&[extent(0, 10), extent(109, 10)], volume);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].end, fraction(119, volume));
        let apart = extents_to_marks(&[extent(0, 10), extent(110, 10)], volume);
        assert_eq!(apart.len(), 2);
    }

    #[test]
    fn touching_and_overlapping_extents_merge() {
        let marks = extents_to_marks(
            &[extent(0, 100), extent(100, 100), extent(50, 400)],
            10_000_000,
        );
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].end, fraction(450, 10_000_000));
    }

    #[test]
    fn a_very_fragmented_file_stays_bounded() {
        let volume = 1u64 << 40;
        let extents: Vec<Extent> = (0..100_000u64)
            .map(|i| extent(i * (volume / 100_000), 4096))
            .collect();
        let marks = extents_to_marks(&extents, volume);
        assert!(marks.len() <= 4096, "{} runs", marks.len());
    }

    #[test]
    fn no_extents_no_marks() {
        assert!(extents_to_marks(&[], 1000).is_empty());
        assert!(extents_to_marks(&[extent(0, 10)], 0).is_empty());
    }

    fn spans(marks: &[MarkRun], width: f32, min: f32) -> Vec<(f32, f32)> {
        let mut out = Vec::new();
        pixel_spans(marks, width, min, |a, b| out.push((a, b)));
        out
    }

    #[test]
    fn a_tiny_mark_is_at_least_the_minimum_wide() {
        let out = spans(&[mark(0.5, 0.500_000_1)], 1000.0, 2.0);
        assert_eq!(out, vec![(500.0, 502.0)]);
    }

    #[test]
    fn a_wide_mark_keeps_its_width() {
        let out = spans(&[mark(0.25, 0.5)], 1000.0, 2.0);
        assert_eq!(out, vec![(250.0, 500.0)]);
    }

    #[test]
    fn marks_overlapping_after_the_minimum_are_painted_once() {
        // 500.0..502.0 and 501.0..503.0 once widened.
        let out = spans(&[mark(0.5, 0.5), mark(0.501, 0.501)], 1000.0, 2.0);
        assert_eq!(out, vec![(500.0, 503.0)]);
        // Far apart stay apart.
        let out = spans(&[mark(0.1, 0.1), mark(0.9, 0.9)], 1000.0, 2.0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_mark_at_the_right_end_stays_inside_the_bar() {
        let out = spans(&[mark(0.999_999, 1.0)], 100.0, 2.0);
        assert_eq!(out, vec![(98.0, 100.0)]);
    }

    #[test]
    fn a_bar_of_no_width_paints_nothing() {
        assert!(spans(&[mark(0.0, 1.0)], 0.0, 2.0).is_empty());
        assert!(spans(&[mark(0.0, 1.0)], -3.0, 2.0).is_empty());
        assert!(spans(&[mark(0.0, 1.0)], f32::NAN, 2.0).is_empty());
    }

    #[test]
    fn a_bar_narrower_than_the_minimum_is_filled() {
        assert_eq!(spans(&[mark(0.4, 0.4)], 1.0, 2.0), vec![(0.0, 1.0)]);
    }

    #[test]
    fn no_marks_no_spans() {
        assert!(spans(&[], 500.0, 2.0).is_empty());
    }

    fn key(source: u32, node: u64) -> Option<(SourceId, NodeId)> {
        Some((SourceId::for_test(source), NodeId(node)))
    }

    #[test]
    fn a_current_result_is_kept() {
        let mut target = MarkTarget::default();
        let Retarget::Fetch(generation) = target.retarget(key(1, 10)) else {
            panic!("a new target needs a fetch");
        };
        assert!(target.accept(generation, vec![mark(0.1, 0.2)]));
        assert_eq!(
            target.runs_for(SourceId::for_test(1)).map(|r| r.len()),
            Some(1)
        );
        // Another volume's bar gets nothing.
        assert!(target.runs_for(SourceId::for_test(2)).is_none());
    }

    #[test]
    fn a_stale_result_is_dropped() {
        let mut target = MarkTarget::default();
        let Retarget::Fetch(first) = target.retarget(key(1, 10)) else {
            panic!("fetch");
        };
        let Retarget::Fetch(second) = target.retarget(key(1, 11)) else {
            panic!("fetch");
        };
        assert_ne!(first, second);
        assert!(!target.accept(first, vec![mark(0.1, 0.2)]));
        assert!(target.runs_for(SourceId::for_test(1)).is_none());
        assert!(target.accept(second, vec![mark(0.3, 0.4)]));
        assert_eq!(
            target.runs_for(SourceId::for_test(1)).map(|r| r[0]),
            Some(mark(0.3, 0.4))
        );
    }

    #[test]
    fn changing_the_target_clears_the_marks_at_once() {
        let mut target = MarkTarget::default();
        let Retarget::Fetch(generation) = target.retarget(key(1, 10)) else {
            panic!("fetch");
        };
        target.accept(generation, vec![mark(0.1, 0.2)]);
        assert_eq!(target.retarget(key(1, 10)), Retarget::Same);
        assert!(target.runs_for(SourceId::for_test(1)).is_some());
        assert_eq!(target.retarget(None), Retarget::Idle);
        assert!(target.runs_for(SourceId::for_test(1)).is_none());
        // The read for the old target lands after the selection was cleared.
        assert!(!target.accept(generation, vec![mark(0.1, 0.2)]));
        assert_eq!(target.retarget(None), Retarget::Same);
    }

    #[test]
    fn going_back_to_a_file_is_a_new_request() {
        let mut target = MarkTarget::default();
        let Retarget::Fetch(first) = target.retarget(key(1, 10)) else {
            panic!("fetch");
        };
        target.retarget(key(1, 11));
        let Retarget::Fetch(again) = target.retarget(key(1, 10)) else {
            panic!("fetch");
        };
        assert_ne!(first, again);
        assert!(!target.accept(first, vec![mark(0.0, 0.1)]));
    }

    #[test]
    fn the_cache_asks_once_per_source() {
        let mut cache = MapCache::default();
        let a = SourceId::for_test(1);
        let b = SourceId::for_test(2);
        assert_eq!(cache.state(a), None);
        assert!(cache.request(a));
        assert_eq!(cache.state(a), Some(&MapState::InFlight));
        assert!(!cache.request(a), "in flight: no second request");
        assert!(cache.runs(a).is_none());
        assert!(cache.request(b), "another source is independent");

        let runs: Arc<[ShadeRun]> = Arc::from(vec![run(0.0, 1.0, 3)]);
        cache.finish(a, Some(Arc::clone(&runs)));
        assert_eq!(cache.state(a), Some(&MapState::Ready(Arc::clone(&runs))));
        assert_eq!(cache.runs(a), Some(&runs));
        assert!(!cache.request(a), "done: no second request");
    }

    #[test]
    fn a_failed_map_is_not_retried() {
        let mut cache = MapCache::default();
        let a = SourceId::for_test(1);
        assert!(cache.request(a));
        cache.finish(a, None);
        assert_eq!(cache.state(a), Some(&MapState::Failed));
        assert!(cache.runs(a).is_none());
        assert!(!cache.request(a));
    }

    #[test]
    fn a_result_for_a_source_never_requested_is_ignored() {
        let mut cache = MapCache::default();
        cache.finish(
            SourceId::for_test(9),
            Some(Arc::from(vec![run(0.0, 1.0, 1)])),
        );
        assert_eq!(cache.state(SourceId::for_test(9)), None);
    }

    /// A filesystem that answers only the strip's two questions.
    struct Fake {
        map: Option<AllocationMap>,
        data: Result<Vec<Extent>, ()>,
        resource: Result<Vec<Extent>, ()>,
    }

    impl FileSystem for Fake {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("fake")
        }
        fn root(&self) -> NodeId {
            NodeId(2)
        }
        fn metadata(&self, _node: NodeId) -> EngineResult<Metadata> {
            Err(Error::unsupported("fake"))
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> EngineResult<Option<NodeId>> {
            Err(Error::unsupported("fake"))
        }
        fn read_dir(&self, _dir: NodeId) -> EngineResult<Box<dyn DirStream>> {
            Err(Error::unsupported("fake"))
        }
        fn extents(&self, _node: NodeId, stream: Stream) -> EngineResult<Vec<Extent>> {
            let answer = match stream {
                Stream::Data => &self.data,
                _ => &self.resource,
            };
            answer.clone().map_err(|()| Error::unsupported("extents"))
        }
        fn allocation_map(&self, buckets: usize) -> EngineResult<AllocationMap> {
            assert_eq!(buckets, 512);
            self.map.clone().ok_or_else(|| Error::unsupported("map"))
        }
    }

    #[test]
    fn shading_comes_from_the_filesystems_map() {
        let fs = Fake {
            map: Some(map(0, 100, &[255, 0])),
            data: Ok(Vec::new()),
            resource: Ok(Vec::new()),
        };
        let runs = load_shading(&fs, 100).expect("a map");
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn no_map_and_an_empty_map_give_no_shading() {
        let none = Fake {
            map: None,
            data: Ok(Vec::new()),
            resource: Ok(Vec::new()),
        };
        assert!(load_shading(&none, 100).is_none());
        let empty = Fake {
            map: Some(map(0, 100, &[])),
            data: Ok(Vec::new()),
            resource: Ok(Vec::new()),
        };
        assert!(load_shading(&empty, 100).is_none());
    }

    #[test]
    fn both_forks_are_marked() {
        let fs = Fake {
            map: None,
            data: Ok(vec![extent(0, 100)]),
            resource: Ok(vec![extent(500, 100)]),
        };
        let marks = load_marks(&fs, NodeId(20), 1000);
        assert_eq!(marks, vec![mark(0.0, 0.1), mark(0.5, 0.6)]);
    }

    #[test]
    fn a_stream_that_fails_adds_no_marks_but_keeps_the_other() {
        let data_fails = Fake {
            map: None,
            data: Err(()),
            resource: Ok(vec![extent(500, 100)]),
        };
        assert_eq!(
            load_marks(&data_fails, NodeId(20), 1000),
            vec![mark(0.5, 0.6)]
        );
        let resource_fails = Fake {
            map: None,
            data: Ok(vec![extent(0, 100)]),
            resource: Err(()),
        };
        assert_eq!(
            load_marks(&resource_fails, NodeId(20), 1000),
            vec![mark(0.0, 0.1)]
        );
        let both_fail = Fake {
            map: None,
            data: Err(()),
            resource: Err(()),
        };
        assert!(load_marks(&both_fail, NodeId(20), 1000).is_empty());
    }
}
