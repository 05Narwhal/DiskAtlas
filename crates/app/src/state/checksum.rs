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

//! The Checksum section of the Properties window (`r10-everyday.md` section 7, R18.3): a SHA-256
//! of one file's data, calculated on the background executor in 1 MiB steps.
//!
//! Everything here is plain data and plain functions with no window in them, so it is tested
//! without one. The window's entity (`state/properties.rs`) owns the cancel flag and the two
//! tasks; this module has the states the section moves through, the step loop, and the shared
//! counter the loop publishes into and the window reads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use disk_atlas_lib::error::Error;
use disk_atlas_lib::io::block::BlockSource;
use gpui::SharedString;
use sha2::{Digest, Sha256};

use crate::format;

/// The bytes read and hashed in one step. The cancel flag is checked between steps.
pub const STEP: usize = 1 << 20;

/// How often the window looks at the progress counter: at most ten updates a second, however
/// fast the steps go.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Where the calculation stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Not started, or cancelled: the button is offered.
    Idle,
    /// Hashing. `total` is 0 until the file is open.
    Running { done: u64, total: u64 },
    /// 64 lowercase hex characters.
    Done(SharedString),
    /// It stopped with this reason; the button is offered again.
    Failed(SharedString),
}

/// The state of the Checksum section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    /// The file is on a disk Disk Atlas reads: only its data fork is hashed, and the row says so.
    pub data_fork: bool,
    pub phase: Phase,
}

impl Checksum {
    /// A section that has not been started.
    pub fn idle(data_fork: bool) -> Self {
        Self {
            data_fork,
            phase: Phase::Idle,
        }
    }

    /// The label of the row.
    pub fn label(&self) -> &'static str {
        if self.data_fork {
            "SHA-256 of the data fork"
        } else {
            "SHA-256"
        }
    }

    /// The digest, once calculated.
    pub fn digest(&self) -> Option<&SharedString> {
        match &self.phase {
            Phase::Done(digest) => Some(digest),
            _ => None,
        }
    }
}

/// "40.2 MB of 1.2 GB (3%)"; "Starting..." before the size is known.
pub fn running_text(done: u64, total: u64) -> String {
    if total == 0 {
        return "Starting\u{2026}".to_owned();
    }
    let done = done.min(total);
    let percent = u128::from(done) * 100 / u128::from(total);
    format!(
        "{} of {} ({percent}%)",
        format::size(done),
        format::size(total)
    )
}

/// `bytes` as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// How a calculation ended when it did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    Cancelled,
}

/// The sentence for a read that failed.
fn read_failure(error: &Error) -> SharedString {
    format!("The file could not be read ({error}).").into()
}

/// The counter the calculation publishes into and the window reads.
#[derive(Debug, Default)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
}

impl Progress {
    /// Records how far the calculation is.
    pub fn publish(&self, done: u64, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.done.store(done, Ordering::Relaxed);
    }

    /// What the window looks at on each tick: `Some` with the newest figures when they differ
    /// from `shown` (and `shown` becomes them), `None` when nothing moved. However many steps
    /// ran since the last tick, there is one answer.
    pub fn changed_since(&self, shown: &mut (u64, u64)) -> Option<(u64, u64)> {
        let now = (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        );
        if now == *shown {
            return None;
        }
        *shown = now;
        Some(now)
    }
}

/// Hashes the whole of `source` in steps of `step` bytes (at least one). `cancel` is checked
/// before every step and `progress(done, total)` is called after each. Returns the digest, or
/// `Cancelled`, or the reason a read failed.
///
/// The length is read once, at the start, and every loop iteration moves `done` forward by at
/// least one byte towards it, so the loop is bounded by the length. A source that reads short or
/// changes its length while being read is an error, never a panic.
pub fn hash_source(
    source: &dyn BlockSource,
    cancel: &AtomicBool,
    step: usize,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Outcome, SharedString> {
    let total = source.len();
    let step = step.max(1);
    // Never allocate more than the file holds: a small file is not a megabyte of zeros.
    let capacity = usize::try_from(total).unwrap_or(usize::MAX).min(step);
    let mut buffer = vec![0u8; capacity];
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    progress(done, total);
    while done < total {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Outcome::Cancelled);
        }
        let left = total.saturating_sub(done);
        let len = usize::try_from(left).unwrap_or(usize::MAX).min(capacity);
        let Some(chunk) = buffer.get_mut(..len) else {
            return Err("The file could not be read (a step was out of range).".into());
        };
        source
            .read_bulk(done, chunk)
            .map_err(|error| read_failure(&error))?;
        hasher.update(&*chunk);
        done = done.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
        progress(done, total);
    }
    if source.len() != total {
        return Err("The file changed while it was being read.".into());
    }
    Ok(Outcome::Done(hex(&hasher.finalize())))
}

/// The whole job as the background executor runs it: `opened` is the result of opening the
/// file's data, `progress` gets its length as soon as it is known and every step after. Blocking.
pub fn run(
    opened: Result<Arc<dyn BlockSource>, Error>,
    cancel: &AtomicBool,
    progress: &Progress,
) -> Result<Outcome, SharedString> {
    let source = opened.map_err(|error| read_failure(&error))?;
    hash_source(source.as_ref(), cancel, STEP, &mut |done, total| {
        progress.publish(done, total);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::io::block::MemSource;

    /// Bytes that are not the same all the way through, `len` of them.
    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn digest_of(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn hash(bytes: Vec<u8>) -> Result<Outcome, SharedString> {
        let source = MemSource::new(bytes);
        hash_source(&source, &AtomicBool::new(false), STEP, &mut |_, _| {})
    }

    #[test]
    fn the_empty_input_has_the_known_digest() {
        assert_eq!(
            hash(Vec::new()),
            Ok(Outcome::Done(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into()
            ))
        );
    }

    #[test]
    fn abc_has_the_known_digest() {
        assert_eq!(
            hash(b"abc".to_vec()),
            Ok(Outcome::Done(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()
            ))
        );
    }

    #[test]
    fn sizes_around_the_step_match_the_whole_buffer_digest() {
        for len in [STEP - 1, STEP, STEP + 1, 3 * STEP + 7] {
            let data = bytes(len);
            let want = digest_of(&data);
            assert_eq!(hash(data), Ok(Outcome::Done(want)), "length {len}");
        }
    }

    #[test]
    fn the_digest_is_sixty_four_lowercase_hex_characters() {
        let Ok(Outcome::Done(digest)) = hash(bytes(10)) else {
            panic!("expected a digest");
        };
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }

    #[test]
    fn steps_are_reported_and_a_small_file_is_one_step() {
        let source = MemSource::new(bytes(3 * STEP + 7));
        let mut seen = Vec::new();
        hash_source(
            &source,
            &AtomicBool::new(false),
            STEP,
            &mut |done, total| {
                seen.push((done, total));
            },
        )
        .expect("hashes");
        let total = (3 * STEP + 7) as u64;
        assert_eq!(seen.first(), Some(&(0, total)));
        assert_eq!(seen.last(), Some(&(total, total)));
        // The start, three whole steps and the remainder.
        assert_eq!(seen.len(), 5);
        assert!(seen.windows(2).all(|pair| pair[0].0 < pair[1].0));

        let mut small = Vec::new();
        let source = MemSource::new(bytes(10));
        hash_source(
            &source,
            &AtomicBool::new(false),
            STEP,
            &mut |done, total| {
                small.push((done, total));
            },
        )
        .expect("hashes");
        assert_eq!(small, [(0, 10), (10, 10)]);
    }

    #[test]
    fn a_step_of_zero_still_finishes() {
        let source = MemSource::new(bytes(5));
        let result = hash_source(&source, &AtomicBool::new(false), 0, &mut |_, _| {});
        assert_eq!(result, Ok(Outcome::Done(digest_of(&bytes(5)))));
    }

    #[test]
    fn cancelling_between_steps_stops_and_says_so() {
        let source = MemSource::new(bytes(3 * STEP));
        let cancel = AtomicBool::new(false);
        let mut steps = 0;
        let result = hash_source(&source, &cancel, STEP, &mut |done, _| {
            if done > 0 {
                steps += 1;
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(result, Ok(Outcome::Cancelled));
        assert_eq!(steps, 1, "no step ran after the flag was set");
    }

    #[test]
    fn a_flag_set_before_the_start_reads_nothing() {
        let source = MemSource::new(bytes(STEP));
        let cancel = AtomicBool::new(true);
        let mut calls = 0;
        let result = hash_source(&source, &cancel, STEP, &mut |_, _| calls += 1);
        assert_eq!(result, Ok(Outcome::Cancelled));
        assert_eq!(calls, 1, "only the opening report");
    }

    /// A source that claims `claimed` bytes and fails from `fail_at` on.
    struct Failing {
        claimed: u64,
        fail_at: u64,
    }

    impl BlockSource for Failing {
        fn len(&self) -> u64 {
            self.claimed
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> disk_atlas_lib::error::Result<()> {
            if offset >= self.fail_at {
                return Err(Error::OutOfBounds {
                    offset,
                    len: buf.len() as u64,
                    size: self.fail_at,
                });
            }
            buf.fill(7);
            Ok(())
        }
    }

    #[test]
    fn a_read_that_fails_midway_ends_in_the_failed_state_with_the_error() {
        let source = Failing {
            claimed: 3 * STEP as u64,
            fail_at: STEP as u64,
        };
        let result = hash_source(&source, &AtomicBool::new(false), STEP, &mut |_, _| {});
        let Err(reason) = result else {
            panic!("expected a failure");
        };
        assert!(
            reason.starts_with("The file could not be read ("),
            "{reason}"
        );
    }

    #[test]
    fn a_source_that_claims_more_than_it_holds_fails_instead_of_panicking() {
        // Claims 2^60 bytes: the first read past the real end fails, and nothing that large is
        // allocated.
        let source = Failing {
            claimed: 1 << 60,
            fail_at: 10,
        };
        assert!(hash_source(&source, &AtomicBool::new(false), STEP, &mut |_, _| {}).is_err());
    }

    /// A source whose length grows after the first read.
    struct Growing {
        inner: MemSource,
        reads: AtomicU64,
    }

    impl BlockSource for Growing {
        fn len(&self) -> u64 {
            self.inner.len() + self.reads.load(Ordering::Relaxed)
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> disk_atlas_lib::error::Result<()> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.read_at(offset, buf)
        }
    }

    #[test]
    fn a_file_that_changes_size_is_an_error() {
        let source = Growing {
            inner: MemSource::new(bytes(100)),
            reads: AtomicU64::new(0),
        };
        let result = hash_source(&source, &AtomicBool::new(false), STEP, &mut |_, _| {});
        assert_eq!(
            result,
            Err("The file changed while it was being read.".into())
        );
    }

    #[test]
    fn a_file_that_will_not_open_is_the_failed_state() {
        let opened: Result<Arc<dyn BlockSource>, Error> = Err(Error::NotFound { node: 9 });
        let progress = Progress::default();
        let result = run(opened, &AtomicBool::new(false), &progress);
        let Err(reason) = result else {
            panic!("expected a failure");
        };
        assert!(
            reason.starts_with("The file could not be read ("),
            "{reason}"
        );
    }

    #[test]
    fn run_publishes_its_progress_and_the_digest() {
        let data = bytes(2 * STEP + 3);
        let want = digest_of(&data);
        let opened: Result<Arc<dyn BlockSource>, Error> = Ok(Arc::new(MemSource::new(data)));
        let progress = Progress::default();
        let result = run(opened, &AtomicBool::new(false), &progress);
        assert_eq!(result, Ok(Outcome::Done(want)));
        let total = (2 * STEP + 3) as u64;
        assert_eq!(progress.changed_since(&mut (0, 0)), Some((total, total)));
    }

    #[test]
    fn progress_is_one_update_per_look_however_many_steps_ran() {
        let progress = Progress::default();
        let mut shown = (0, 0);
        assert_eq!(progress.changed_since(&mut shown), None);
        // Three steps between two looks: one answer, the newest.
        for done in [1, 2, 3] {
            progress.publish(done, 10);
        }
        assert_eq!(progress.changed_since(&mut shown), Some((3, 10)));
        assert_eq!(shown, (3, 10));
        // Nothing moved: nothing to draw.
        assert_eq!(progress.changed_since(&mut shown), None);
        // The looks themselves come at most ten times a second.
        assert!(PROGRESS_INTERVAL >= Duration::from_millis(100));
    }

    #[test]
    fn the_running_text_says_how_far_with_a_percentage() {
        assert_eq!(running_text(0, 0), "Starting\u{2026}");
        assert_eq!(running_text(512, 1024), "512 bytes of 1 KB (50%)");
        // Past the end is shown as the end, and no sum overflows.
        assert_eq!(running_text(2048, 1024), "1 KB of 1 KB (100%)");
        assert!(running_text(u64::MAX, u64::MAX).ends_with("(100%)"));
    }

    #[test]
    fn the_row_says_data_fork_only_on_a_disk() {
        assert_eq!(Checksum::idle(true).label(), "SHA-256 of the data fork");
        assert_eq!(Checksum::idle(false).label(), "SHA-256");
        assert_eq!(Checksum::idle(false).digest(), None);
        let done = Checksum {
            data_fork: false,
            phase: Phase::Done("ab".into()),
        };
        assert_eq!(done.digest().map(|d| d.as_ref()), Some("ab"));
    }
}
