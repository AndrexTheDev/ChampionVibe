//! Soft-knee, 4x-interpolated true-peak limiting.
//!
//! The device limiter uses a short audio delay so it can see peaks before they reach the output.
//! This finite lookahead necessarily adds latency; it is not a zero-latency processor.

use super::{loudness, StereoFrame};
use std::collections::VecDeque;

const TRUE_PEAK_TAPS: usize = 16;
const TRUE_PEAK_PHASES: usize = 4;
const TRUE_PEAK_CENTER: usize = TRUE_PEAK_TAPS / 2 - 1;
const DEFAULT_LOOKAHEAD_MS: f32 = 5.0;
const DEFAULT_CEILING_DBTP: f32 = -1.0;
const DEFAULT_KNEE_DB: f32 = 6.0;
const DEFAULT_RELEASE_MS: f32 = 60.0;

#[derive(Clone, Copy, Debug)]
struct PeakEntry {
    frame: u64,
    peak: f32,
}

/// Soft-knee gain computer used by the shared post-summation live master limiter.
pub(super) struct SoftKneeGain {
    ceiling_db: f32,
    knee_db: f32,
    release_coefficient: f32,
    gain_db: f32,
}

impl SoftKneeGain {
    pub(super) fn new(sample_rate: u32, ceiling_db: f32, knee_db: f32, release_ms: f32) -> Self {
        let sample_rate = sample_rate.max(1) as f32;
        let release_seconds = release_ms.max(1.0) / 1_000.0;
        let release_coefficient = (-1.0 / (sample_rate * release_seconds)).exp();
        Self {
            ceiling_db: ceiling_db.clamp(-12.0, -0.1),
            knee_db: knee_db.clamp(0.0, 18.0),
            release_coefficient,
            gain_db: 0.0,
        }
    }

    /// Return the current linear gain for a measured (linear) peak. Gain reduction attacks
    /// immediately; recovery follows the configured release, so it cannot overshoot the ceiling.
    #[inline]
    pub(super) fn gain_for_peak(&mut self, peak: f32) -> f32 {
        let peak_db = if peak.is_finite() && peak > 1.0e-12 {
            20.0 * peak.log10()
        } else {
            -120.0
        };
        let desired_db = soft_knee_reduction_db(peak_db, self.ceiling_db, self.knee_db);
        if desired_db < self.gain_db {
            self.gain_db = desired_db;
        } else {
            self.gain_db = desired_db + (self.gain_db - desired_db) * self.release_coefficient;
        }
        10.0_f32.powf(self.gain_db / 20.0)
    }

    #[inline]
    fn gain_reduction_db(&self) -> f32 {
        (-self.gain_db).max(0.0)
    }
}

fn soft_knee_reduction_db(level_db: f32, ceiling_db: f32, knee_db: f32) -> f32 {
    if knee_db <= f32::EPSILON {
        return (ceiling_db - level_db).min(0.0);
    }
    let over = level_db - ceiling_db;
    let half_knee = knee_db * 0.5;
    if over <= -half_knee {
        0.0
    } else if over >= half_knee {
        -over
    } else {
        let transition = over + half_knee;
        -(transition * transition) / (2.0 * knee_db)
    }
}

/// Fixed-storage live limiter. `process` returns silence for the initial lookahead interval and
/// then emits the delayed frame after applying a 4x windowed-sinc peak estimate over the window.
pub(super) struct LookaheadLimiter {
    /// Retained for the latency helpers and this module's tests; the sample loop itself only
    /// needs the derived state below.
    #[cfg_attr(not(test), allow(dead_code))]
    sample_rate: u32,
    latency_frames: usize,
    delay: Vec<StereoFrame>,
    history: VecDeque<[f64; 2]>,
    coefficients: [[f64; TRUE_PEAK_TAPS]; TRUE_PEAK_PHASES],
    peak_window: VecDeque<PeakEntry>,
    gain: SoftKneeGain,
    frames_seen: u64,
}

impl LookaheadLimiter {
    pub(super) fn latency_ms_for_sample_rate(sample_rate: u32) -> f32 {
        if sample_rate == 0 {
            return 0.0;
        }
        let lookahead =
            ((sample_rate as f32 * DEFAULT_LOOKAHEAD_MS / 1_000.0).round() as usize).max(1);
        (lookahead + TRUE_PEAK_CENTER + 1) as f32 * 1_000.0 / sample_rate as f32
    }

    pub(super) fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1);
        let lookahead_frames =
            ((sample_rate as f32 * DEFAULT_LOOKAHEAD_MS / 1_000.0).round() as usize).max(1);
        // The centered interpolator needs eight more input samples before its estimate is final.
        let latency_frames = lookahead_frames + TRUE_PEAK_CENTER + 1;
        let mut history = VecDeque::with_capacity(TRUE_PEAK_TAPS);
        for _ in 0..TRUE_PEAK_CENTER {
            history.push_back([0.0; 2]);
        }
        Self {
            sample_rate,
            latency_frames,
            delay: vec![StereoFrame::default(); latency_frames + 1],
            history,
            coefficients: loudness::true_peak_interpolation_coefficients(),
            peak_window: VecDeque::with_capacity(
                lookahead_frames.saturating_mul(2).saturating_add(32),
            ),
            gain: SoftKneeGain::new(
                sample_rate,
                DEFAULT_CEILING_DBTP,
                DEFAULT_KNEE_DB,
                DEFAULT_RELEASE_MS,
            ),
            frames_seen: 0,
        }
    }

    #[cfg(test)]
    #[inline]
    pub(super) fn process(&mut self, input: StereoFrame) -> StereoFrame {
        self.process_with_reduction(input).0
    }

    #[inline]
    pub(super) fn process_with_reduction(&mut self, input: StereoFrame) -> (StereoFrame, f32) {
        let input = StereoFrame {
            left: finite_sample(input.left),
            right: finite_sample(input.right),
        };
        let frame_index = self.frames_seen;
        let write_index = (frame_index % self.delay.len() as u64) as usize;
        self.delay[write_index] = input;
        self.push_true_peak(input, frame_index);

        let Some(output_index) = frame_index.checked_sub(self.latency_frames as u64) else {
            self.frames_seen = self.frames_seen.saturating_add(1);
            return (StereoFrame::default(), 0.0);
        };
        while self
            .peak_window
            .front()
            .is_some_and(|entry| entry.frame < output_index)
        {
            self.peak_window.pop_front();
        }
        let peak = self.peak_window.front().map_or(0.0, |entry| entry.peak);
        let gain = self.gain.gain_for_peak(peak);
        let gain_reduction_db = self.gain.gain_reduction_db();
        let delayed_index = (output_index % self.delay.len() as u64) as usize;
        let delayed = self.delay[delayed_index];
        self.frames_seen = self.frames_seen.saturating_add(1);
        (
            StereoFrame {
                left: delayed.left * gain,
                right: delayed.right * gain,
            },
            gain_reduction_db,
        )
    }

    #[cfg(test)]
    pub(super) fn latency_frames(&self) -> usize {
        self.latency_frames
    }

    #[cfg(test)]
    pub(super) fn latency_ms(&self) -> f32 {
        self.latency_frames as f32 * 1_000.0 / self.sample_rate as f32
    }

    fn push_true_peak(&mut self, input: StereoFrame, input_index: u64) {
        if self.history.len() == TRUE_PEAK_TAPS {
            self.history.pop_front();
        }
        self.history
            .push_back([f64::from(input.left), f64::from(input.right)]);
        if self.history.len() != TRUE_PEAK_TAPS {
            return;
        }

        let center_index = input_index.saturating_sub(TRUE_PEAK_CENTER as u64 + 1);
        let mut peak = 0.0_f64;
        for phase in 0..TRUE_PEAK_PHASES {
            let mut left = 0.0_f64;
            let mut right = 0.0_f64;
            for (index, frame) in self.history.iter().enumerate() {
                let coefficient = self.coefficients[phase][index];
                left += frame[0] * coefficient;
                right += frame[1] * coefficient;
            }
            peak = peak.max(left.abs()).max(right.abs());
        }
        self.push_peak(PeakEntry {
            frame: center_index,
            peak: peak.min(f64::from(f32::MAX)) as f32,
        });
    }

    fn push_peak(&mut self, entry: PeakEntry) {
        while self
            .peak_window
            .back()
            .is_some_and(|previous| previous.peak <= entry.peak)
        {
            self.peak_window.pop_back();
        }
        self.peak_window.push_back(entry);
    }
}

#[inline]
fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{soft_knee_reduction_db, LookaheadLimiter, SoftKneeGain};
    use crate::audio::StereoFrame;

    #[test]
    fn soft_knee_is_smooth_and_reaches_the_ceiling_above_the_knee() {
        assert_eq!(soft_knee_reduction_db(-5.0, -1.0, 6.0), 0.0);
        let at_threshold = soft_knee_reduction_db(-1.0, -1.0, 6.0);
        assert!((at_threshold + 0.75).abs() < 1.0e-6);
        let above = soft_knee_reduction_db(6.0, -1.0, 6.0);
        assert!((above + 7.0).abs() < 1.0e-6);
    }

    #[test]
    fn gain_reduction_attacks_immediately_and_release_recovers_smoothly() {
        let mut gain = SoftKneeGain::new(48_000, -1.0, 6.0, 60.0);
        let limited = gain.gain_for_peak(2.0);
        assert!(limited < 0.5);
        let recovering = gain.gain_for_peak(0.1);
        assert!(recovering > limited && recovering < 1.0);
    }

    #[test]
    fn lookahead_limiter_has_finite_latency_and_limits_overrange_steady_signal() {
        assert_eq!(LookaheadLimiter::latency_ms_for_sample_rate(0), 0.0);
        let mut limiter = LookaheadLimiter::new(48_000);
        assert!(limiter.latency_ms() >= 5.0 && limiter.latency_ms() <= 6.0);
        let mut output = Vec::new();
        for _ in 0..4_000 {
            output.push(limiter.process(StereoFrame {
                left: 1.5,
                right: -1.5,
            }));
        }
        let ceiling = 10.0_f32.powf(-1.0 / 20.0);
        let peak = output
            .iter()
            .skip(limiter.latency_frames())
            .map(|frame| frame.left.abs().max(frame.right.abs()))
            .fold(0.0, f32::max);
        assert!(peak <= ceiling + 1.0e-4, "peak {peak} exceeds {ceiling}");
    }

    #[test]
    fn non_finite_audio_is_replaced_with_silence() {
        let mut limiter = LookaheadLimiter::new(44_100);
        let mut last = StereoFrame::default();
        for _ in 0..2_000 {
            last = limiter.process(StereoFrame {
                left: f32::NAN,
                right: f32::INFINITY,
            });
        }
        assert_eq!(last.left, 0.0);
        assert_eq!(last.right, 0.0);
    }
}
