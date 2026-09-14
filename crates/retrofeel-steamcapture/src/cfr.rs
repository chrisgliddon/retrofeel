//! Explicit CFR resampling for ScreenCaptureKit's irregular frame delivery.
//!
//! The encoded recording is a constant-rate master. SCK timestamps are not
//! discarded: every emitted grid tick carries the selected source timestamp in
//! a [`FrameMapEntry`], while superseded frames and repeated grid ticks are
//! counted as capture-quality telemetry.

use retrofeel_types::FrameMapEntry;

use crate::CapturedFrame;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CfrStats {
    pub source_frames_received: u64,
    pub source_frames_discarded: u64,
    pub grid_duplicates: u64,
}

#[derive(Debug, Clone)]
pub struct CfrTick {
    pub encoded_frame: u64,
    pub elapsed_us: u64,
    pub source: CapturedFrame,
    pub frame_map: FrameMapEntry,
}

#[derive(Debug, Clone)]
struct SourceFrame {
    frame: CapturedFrame,
    used_by_grid: bool,
}

/// Selects the latest SCK frame at or before each CFR grid instant.
///
/// The first source frame establishes time zero and immediately supplies
/// encoded frame zero. Later source frames only become selectable at a grid
/// time at or after their true source PTS; a source with no such grid tick is
/// recorded as an intentional discard rather than silently disappearing.
#[derive(Debug, Clone)]
pub struct CfrResampler {
    fps: u32,
    first_source_pts_us: Option<u64>,
    source_grid_origin_elapsed_us: u64,
    next_encoded_frame: u64,
    latest: Option<SourceFrame>,
    previous_emitted_source: Option<u64>,
    pending_discarded_sources: u64,
    stats: CfrStats,
}

impl CfrResampler {
    pub fn new(fps: u32) -> Self {
        Self {
            fps: fps.max(1),
            first_source_pts_us: None,
            source_grid_origin_elapsed_us: 0,
            next_encoded_frame: 0,
            latest: None,
            previous_emitted_source: None,
            pending_discarded_sources: 0,
            stats: CfrStats::default(),
        }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn first_source_pts_us(&self) -> Option<u64> {
        self.first_source_pts_us
    }

    pub fn stats(&self) -> CfrStats {
        self.stats
    }

    /// Start a fresh source-clock segment after an intentional recording
    /// pause while preserving the encoded CFR frame index. The next delivered
    /// SCK frame supplies the next encoded grid tick; pause wall time never
    /// becomes invented duplicated video.
    pub fn rebase_after_pause(&mut self) {
        if self
            .latest
            .as_ref()
            .is_some_and(|source| !source.used_by_grid)
        {
            self.stats.source_frames_discarded =
                self.stats.source_frames_discarded.saturating_add(1);
        }
        self.first_source_pts_us = None;
        self.source_grid_origin_elapsed_us = self.next_encoded_elapsed_us();
        self.latest = None;
        self.previous_emitted_source = None;
        self.pending_discarded_sources = 0;
    }

    /// Consume one source callback and return each newly due encoded CFR tick.
    pub fn push(&mut self, source: CapturedFrame) -> Vec<CfrTick> {
        self.stats.source_frames_received = self.stats.source_frames_received.saturating_add(1);

        let Some(first_source_pts_us) = self.first_source_pts_us else {
            self.first_source_pts_us = Some(source.source_pts_us);
            self.latest = Some(SourceFrame {
                frame: source,
                used_by_grid: false,
            });
            return self.emit_due_through(0);
        };

        let source_elapsed_us = source.source_pts_us.saturating_sub(first_source_pts_us);
        // The just-arrived source is not eligible for ticks before its source
        // PTS. Flush those using the last source first.
        let mut ticks = self.emit_before(source_elapsed_us);

        if let Some(previous) = self.latest.take() {
            if !previous.used_by_grid {
                self.pending_discarded_sources = self.pending_discarded_sources.saturating_add(1);
                self.stats.source_frames_discarded =
                    self.stats.source_frames_discarded.saturating_add(1);
            }
        }
        self.latest = Some(SourceFrame {
            frame: source,
            used_by_grid: false,
        });
        ticks.extend(self.emit_due_through(source_elapsed_us));
        ticks
    }

    /// Account for a final source frame that never reached an encoded tick.
    /// No artificial tail frame is emitted at stop: doing so would invent a
    /// duration absent from the SCK source timeline.
    pub fn finish(&mut self) -> CfrStats {
        if self
            .latest
            .as_ref()
            .is_some_and(|source| !source.used_by_grid)
        {
            self.stats.source_frames_discarded =
                self.stats.source_frames_discarded.saturating_add(1);
            self.pending_discarded_sources = self.pending_discarded_sources.saturating_add(1);
            if let Some(source) = self.latest.as_mut() {
                source.used_by_grid = true;
            }
        }
        self.stats
    }

    fn emit_before(&mut self, exclusive_elapsed_us: u64) -> Vec<CfrTick> {
        let mut ticks = Vec::new();
        while self.next_tick_elapsed_us() < exclusive_elapsed_us {
            if let Some(tick) = self.emit_one() {
                ticks.push(tick);
            } else {
                break;
            }
        }
        ticks
    }

    fn emit_due_through(&mut self, inclusive_elapsed_us: u64) -> Vec<CfrTick> {
        let mut ticks = Vec::new();
        while self.next_tick_elapsed_us() <= inclusive_elapsed_us {
            if let Some(tick) = self.emit_one() {
                ticks.push(tick);
            } else {
                break;
            }
        }
        ticks
    }

    fn next_tick_elapsed_us(&self) -> u64 {
        self.next_encoded_elapsed_us()
            .saturating_sub(self.source_grid_origin_elapsed_us)
    }

    fn next_encoded_elapsed_us(&self) -> u64 {
        ((self.next_encoded_frame as u128 * 1_000_000_u128) / self.fps as u128) as u64
    }

    fn emit_one(&mut self) -> Option<CfrTick> {
        let elapsed_us = self.next_encoded_elapsed_us();
        let source = self.latest.as_mut()?;
        let grid_duplicate = self.previous_emitted_source == Some(source.frame.frame_index);
        if grid_duplicate {
            self.stats.grid_duplicates = self.stats.grid_duplicates.saturating_add(1);
        }
        let encoded_frame = self.next_encoded_frame;
        self.next_encoded_frame = self.next_encoded_frame.saturating_add(1);
        self.previous_emitted_source = Some(source.frame.frame_index);
        source.used_by_grid = true;
        let discarded_source_frames_before = std::mem::take(&mut self.pending_discarded_sources);
        Some(CfrTick {
            encoded_frame,
            elapsed_us,
            source: source.frame.clone(),
            frame_map: FrameMapEntry {
                encoded_frame,
                encoded_pts_us: elapsed_us,
                source_frame: source.frame.frame_index,
                source_pts_us: source.frame.source_pts_us,
                grid_duplicate,
                discarded_source_frames_before,
                writer_dupe: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retrofeel_types::RawHostInput;

    fn source(frame_index: u64, source_pts_us: u64) -> CapturedFrame {
        CapturedFrame {
            frame_index,
            source_pts_us,
            source_mach_us: 0,
            video: None,
            raw_host: RawHostInput::default(),
        }
    }

    #[test]
    fn jittered_source_frames_map_to_a_complete_cfr_grid() {
        let mut resampler = CfrResampler::new(60);
        assert_eq!(resampler.push(source(0, 1_000_000)).len(), 1);
        assert!(resampler.push(source(1, 1_012_000)).is_empty());
        let ticks = resampler.push(source(2, 1_040_000));
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].elapsed_us, 16_666);
        assert_eq!(ticks[1].elapsed_us, 33_333);
        assert_eq!(ticks[0].frame_map.source_frame, 1);
        assert!(ticks[1].frame_map.grid_duplicate);
        assert_eq!(resampler.stats().grid_duplicates, 1);
    }

    #[test]
    fn source_frames_superseded_before_a_tick_are_reported() {
        let mut resampler = CfrResampler::new(60);
        let _ = resampler.push(source(0, 0));
        let _ = resampler.push(source(1, 2_000));
        let _ = resampler.push(source(2, 5_000));
        let ticks = resampler.push(source(3, 20_000));
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].frame_map.source_frame, 2);
        assert_eq!(ticks[0].frame_map.discarded_source_frames_before, 1);
        assert_eq!(resampler.stats().source_frames_discarded, 1);
    }

    #[test]
    fn first_source_establishes_zero_even_when_its_pts_is_nonzero() {
        let mut resampler = CfrResampler::new(30);
        let ticks = resampler.push(source(7, 8_500_000));
        assert_eq!(ticks[0].encoded_frame, 0);
        assert_eq!(ticks[0].elapsed_us, 0);
        assert_eq!(ticks[0].frame_map.source_pts_us, 8_500_000);
    }
}
