//! Streaming WSOLA time stretching for pitch-locked tempo matching.
//!
//! A single shared analysis schedule (selected against the instrumental stem) is used by every
//! active stem, keeping aligned four-stem playback synchronized. All buffering lives on decoder
//! workers; the CPAL callback only consumes the prebuffered SPSC rings.

use super::StereoFrame;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

const WINDOW_MS: f32 = 40.0;
const SEARCH_MS: f32 = 8.0;
const MIN_WINDOW_FRAMES: usize = 128;
const MAX_WINDOW_FRAMES: usize = 8_192;
const SEARCH_CANDIDATES: usize = 384;

#[derive(Default)]
struct ScheduleState {
    offsets: Vec<u64>,
    finished: bool,
}

/// One offset sequence shared across the active stems. Only the instrumental decoder chooses
/// offsets; the other stem workers wait for and apply those exact source positions.
pub(super) struct SharedSchedule {
    state: Mutex<ScheduleState>,
    changed: Condvar,
}

/// Linear 10 ms transition between the live WSOLA and varispeed branches.
/// The target is sampled on the master output callback, so control changes do not require a
/// track reload and do not wait for decoder-ring contents to be regenerated.
pub(super) struct PitchLockTransition {
    wsola_weight: f32,
    step_per_frame: f32,
}

impl PitchLockTransition {
    pub(super) fn new(sample_rate: u32, pitch_lock_enabled: bool) -> Self {
        Self {
            wsola_weight: if pitch_lock_enabled { 1.0 } else { 0.0 },
            step_per_frame: 1.0 / (sample_rate.max(1) as f32 * 0.010),
        }
    }

    #[inline]
    pub(super) fn mix(
        &mut self,
        wsola: StereoFrame,
        varispeed: StereoFrame,
        pitch_lock_enabled: bool,
    ) -> StereoFrame {
        let target = if pitch_lock_enabled { 1.0 } else { 0.0 };
        if self.wsola_weight < target {
            self.wsola_weight = (self.wsola_weight + self.step_per_frame).min(target);
        } else if self.wsola_weight > target {
            self.wsola_weight = (self.wsola_weight - self.step_per_frame).max(target);
        }
        let varispeed_weight = 1.0 - self.wsola_weight;
        StereoFrame {
            left: wsola.left * self.wsola_weight + varispeed.left * varispeed_weight,
            right: wsola.right * self.wsola_weight + varispeed.right * varispeed_weight,
        }
    }

    #[cfg(test)]
    fn weight(&self) -> f32 {
        self.wsola_weight
    }
}

impl SharedSchedule {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(ScheduleState::default()),
            changed: Condvar::new(),
        }
    }

    fn publish(&self, sequence: usize, offset: u64) {
        if let Ok(mut state) = self.state.lock() {
            if state.offsets.len() == sequence {
                state.offsets.push(offset);
            }
            self.changed.notify_all();
        }
    }

    fn wait_for(&self, sequence: usize, cancel: &AtomicBool) -> Option<u64> {
        let mut state = self.state.lock().ok()?;
        while state.offsets.len() <= sequence && !state.finished && !cancel.load(Ordering::Acquire)
        {
            let (next, _) = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .ok()?;
            state = next;
        }
        if cancel.load(Ordering::Acquire) {
            None
        } else {
            state.offsets.get(sequence).copied()
        }
    }

    pub(super) fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.finished = true;
            self.changed.notify_all();
        }
    }
}

pub(super) struct WsolaProcessor {
    sample_rate: u32,
    speed: f32,
    window_frames: usize,
    hop_frames: usize,
    search_frames: usize,
    is_controller: bool,
    schedule: Arc<SharedSchedule>,
    input: VecDeque<StereoFrame>,
    input_base: u64,
    output: VecDeque<StereoFrame>,
    previous_tail: Vec<StereoFrame>,
    previous_start: u64,
    next_sequence: usize,
    first_grain: bool,
    source_finished: bool,
    drained: bool,
}

impl WsolaProcessor {
    pub(super) fn new(
        sample_rate: u32,
        speed: f32,
        is_controller: bool,
        schedule: Arc<SharedSchedule>,
    ) -> Result<Self, String> {
        if !(0.8..=1.2).contains(&speed) || !speed.is_finite() {
            return Err("pitch-locked WSOLA speed must be between 0.80 and 1.20".to_owned());
        }
        let sample_rate = sample_rate.max(1);
        let window_frames = ((sample_rate as f32 * WINDOW_MS / 1_000.0).round() as usize)
            .clamp(MIN_WINDOW_FRAMES, MAX_WINDOW_FRAMES);
        let window_frames = if window_frames % 2 == 0 {
            window_frames
        } else {
            window_frames + 1
        };
        let hop_frames = window_frames / 2;
        let search_frames = ((sample_rate as f32 * SEARCH_MS / 1_000.0).round() as usize)
            .min(hop_frames / 2)
            .max(1);
        Ok(Self {
            sample_rate,
            speed,
            window_frames,
            hop_frames,
            search_frames,
            is_controller,
            schedule,
            input: VecDeque::with_capacity(window_frames * 3),
            input_base: 0,
            output: VecDeque::with_capacity(hop_frames),
            previous_tail: vec![StereoFrame::default(); hop_frames],
            previous_start: 0,
            next_sequence: 0,
            first_grain: false,
            source_finished: false,
            drained: false,
        })
    }

    pub(super) fn next_frame<F>(
        &mut self,
        mut next_input: F,
        cancel: &AtomicBool,
    ) -> Result<Option<StereoFrame>, String>
    where
        F: FnMut() -> Result<Option<StereoFrame>, String>,
    {
        while self.output.is_empty() && !self.drained {
            if cancel.load(Ordering::Acquire) {
                return Ok(None);
            }
            self.generate_hop(&mut next_input, cancel)?;
        }
        Ok(self.output.pop_front())
    }

    fn generate_hop<F>(&mut self, next_input: &mut F, cancel: &AtomicBool) -> Result<(), String>
    where
        F: FnMut() -> Result<Option<StereoFrame>, String>,
    {
        if !self.first_grain {
            self.fill_until(self.window_frames as u64, next_input)?;
            let available_end = self.input_end();
            if available_end == 0 {
                self.finish_controller();
                self.drained = true;
                return Ok(());
            }
            if self.source_finished && available_end < self.window_frames as u64 {
                let mut short = self.input.iter().copied().collect::<Vec<_>>();
                let short_len = short.len();
                let fade_frames = short_len.min((self.sample_rate as usize / 100).max(1));
                let fade_start = short_len.saturating_sub(fade_frames);
                for (index, frame) in short.iter_mut().enumerate().skip(fade_start) {
                    let remaining = short_len.saturating_sub(index + 1);
                    let gain = remaining as f32 / fade_frames.max(1) as f32;
                    frame.left *= gain;
                    frame.right *= gain;
                }
                self.output.extend(short);
                self.finish_controller();
                self.drained = true;
                return Ok(());
            }
            let grain = self.grain_at(0);
            self.output
                .extend(grain.iter().take(self.hop_frames).copied());
            self.previous_tail
                .copy_from_slice(&grain[self.hop_frames..self.window_frames]);
            self.previous_start = 0;
            self.first_grain = true;
            return Ok(());
        }

        let nominal_advance = self.hop_frames as f64 * f64::from(self.speed);
        // Anchor every grain to the cumulative ideal source position rather than the prior
        // correlation offset. Otherwise a small one-sided alignment correction accumulates as
        // an unintended tempo drift over a long track.
        let expected = ((self.next_sequence as f64 + 1.0) * nominal_advance)
            .round()
            .max(1.0) as u64;
        let minimum = expected
            .saturating_sub(self.search_frames as u64)
            .max(self.previous_start.saturating_add(1));
        let maximum = expected.saturating_add(self.search_frames as u64);
        self.fill_until(
            maximum.saturating_add(self.window_frames as u64),
            next_input,
        )?;
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let available_end = self.input_end();
        if available_end <= minimum {
            self.flush_tail();
            self.finish_controller();
            self.drained = true;
            return Ok(());
        }

        let last_candidate = if self.source_finished {
            maximum.min(available_end.saturating_sub(1))
        } else {
            maximum
        };
        if last_candidate < minimum {
            self.flush_tail();
            self.finish_controller();
            self.drained = true;
            return Ok(());
        }

        let selected = if self.is_controller {
            let offset = self.best_offset(minimum, last_candidate, expected);
            self.schedule.publish(self.next_sequence, offset);
            offset
        } else {
            let Some(offset) = self.schedule.wait_for(self.next_sequence, cancel) else {
                self.flush_tail();
                self.drained = true;
                return Ok(());
            };
            offset
        };
        if selected >= available_end {
            self.flush_tail();
            self.finish_controller();
            self.drained = true;
            return Ok(());
        }

        let grain = self.grain_at(selected);
        let denominator = (self.hop_frames + 1) as f32;
        for index in 0..self.hop_frames {
            let fade = (index + 1) as f32 / denominator;
            let previous = self.previous_tail[index];
            let incoming = grain[index];
            self.output.push_back(StereoFrame {
                left: previous.left * (1.0 - fade) + incoming.left * fade,
                right: previous.right * (1.0 - fade) + incoming.right * fade,
            });
        }
        self.previous_tail
            .copy_from_slice(&grain[self.hop_frames..self.window_frames]);
        self.previous_start = selected;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let next_expected = ((self.next_sequence as f64 + 1.0) * nominal_advance)
            .round()
            .max(1.0) as u64;
        let retain_from = next_expected.saturating_sub(self.search_frames as u64);
        self.discard_before(retain_from);
        Ok(())
    }

    fn fill_until<F>(&mut self, end_exclusive: u64, next_input: &mut F) -> Result<(), String>
    where
        F: FnMut() -> Result<Option<StereoFrame>, String>,
    {
        while self.input_end() < end_exclusive && !self.source_finished {
            match next_input()? {
                Some(frame) => self.input.push_back(StereoFrame {
                    left: finite_sample(frame.left),
                    right: finite_sample(frame.right),
                }),
                None => self.source_finished = true,
            }
        }
        Ok(())
    }

    fn input_end(&self) -> u64 {
        self.input_base.saturating_add(self.input.len() as u64)
    }

    fn grain_at(&self, start: u64) -> Vec<StereoFrame> {
        (0..self.window_frames)
            .map(|offset| {
                let index = start.saturating_add(offset as u64);
                index
                    .checked_sub(self.input_base)
                    .and_then(|relative| self.input.get(relative as usize).copied())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn best_offset(&self, minimum: u64, maximum: u64, expected: u64) -> u64 {
        let span = maximum.saturating_sub(minimum);
        let step = (span / SEARCH_CANDIDATES as u64).max(1);
        let stride = (self.hop_frames / 96).max(1);
        let mut best = expected.clamp(minimum, maximum);
        let mut best_score = f64::NEG_INFINITY;
        let mut candidate = minimum;
        loop {
            let mut correlation = 0.0_f64;
            let mut reference_energy = 0.0_f64;
            let mut candidate_energy = 0.0_f64;
            let mut points = 0usize;
            for index in (0..self.hop_frames).step_by(stride) {
                let reference = self.previous_tail[index];
                let source_index = candidate.saturating_add(index as u64);
                let frame = source_index
                    .checked_sub(self.input_base)
                    .and_then(|relative| self.input.get(relative as usize).copied())
                    .unwrap_or_default();
                let reference_mono = f64::from(reference.left + reference.right) * 0.5;
                let candidate_mono = f64::from(frame.left + frame.right) * 0.5;
                correlation += reference_mono * candidate_mono;
                reference_energy += reference_mono * reference_mono;
                candidate_energy += candidate_mono * candidate_mono;
                points += 1;
            }
            let denominator = (reference_energy * candidate_energy).sqrt().max(1.0e-12);
            let normalized = correlation / denominator;
            // Keep the chosen position near the tempo-derived expectation unless alignment wins.
            let distance_penalty =
                candidate.abs_diff(expected) as f64 / (self.search_frames as f64).max(1.0);
            let score = normalized - distance_penalty * 0.002;
            if points > 0 && score > best_score {
                best = candidate;
                best_score = score;
            }
            if candidate >= maximum || maximum.saturating_sub(candidate) < step {
                break;
            }
            candidate = candidate.saturating_add(step);
        }
        best
    }

    fn discard_before(&mut self, frame: u64) {
        let discard = frame
            .saturating_sub(self.input_base)
            .min(self.input.len() as u64) as usize;
        for _ in 0..discard {
            self.input.pop_front();
        }
        self.input_base = self.input_base.saturating_add(discard as u64);
    }

    fn flush_tail(&mut self) {
        if self.previous_tail.is_empty() {
            return;
        }
        for (index, frame) in self.previous_tail.iter().copied().enumerate() {
            let fade = 1.0 - (index as f32 / self.hop_frames as f32);
            self.output.push_back(StereoFrame {
                left: frame.left * fade,
                right: frame.right * fade,
            });
        }
        self.previous_tail.fill(StereoFrame::default());
    }

    fn finish_controller(&self) {
        if self.is_controller {
            self.schedule.finish();
        }
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
    use super::{PitchLockTransition, SharedSchedule, WsolaProcessor};
    use crate::audio::StereoFrame;
    use std::sync::{atomic::AtomicBool, Arc};

    fn stretch(sine_hz: f64, speed: f32, frames: usize) -> Vec<StereoFrame> {
        let schedule = Arc::new(SharedSchedule::new());
        let mut processor = WsolaProcessor::new(48_000, speed, true, schedule).unwrap();
        let mut input_index = 0usize;
        let cancel = AtomicBool::new(false);
        let mut output = Vec::new();
        loop {
            let next = processor
                .next_frame(
                    || {
                        if input_index >= frames {
                            Ok(None)
                        } else {
                            let phase =
                                std::f64::consts::TAU * sine_hz * input_index as f64 / 48_000.0;
                            let sample = phase.sin() as f32 * 0.25;
                            input_index += 1;
                            Ok(Some(StereoFrame {
                                left: sample,
                                right: sample,
                            }))
                        }
                    },
                    &cancel,
                )
                .unwrap();
            match next {
                Some(frame) => output.push(frame),
                None => break,
            }
        }
        output
    }

    #[test]
    fn wsola_speed_range_is_limited_to_twenty_percent() {
        assert!(WsolaProcessor::new(48_000, 0.79, true, Arc::new(SharedSchedule::new())).is_err());
        assert!(WsolaProcessor::new(48_000, 1.21, true, Arc::new(SharedSchedule::new())).is_err());
        assert!(WsolaProcessor::new(48_000, 0.8, true, Arc::new(SharedSchedule::new())).is_ok());
        assert!(WsolaProcessor::new(48_000, 1.2, true, Arc::new(SharedSchedule::new())).is_ok());
    }

    #[test]
    fn faster_and_slower_rates_change_duration_without_sample_rate_pitch_shift() {
        let input_frames = 48_000;
        let faster = stretch(440.0, 1.2, input_frames);
        let slower = stretch(440.0, 0.8, input_frames);
        assert!(
            faster.len() < input_frames * 90 / 100,
            "fast WSOLA emitted {} frames for {input_frames} source frames",
            faster.len()
        );
        assert!(
            slower.len() > input_frames * 110 / 100,
            "slow WSOLA emitted {} frames for {input_frames} source frames",
            slower.len()
        );

        for output in [&faster, &slower] {
            let crossings = output
                .windows(2)
                .skip(2_000)
                .take(20_000)
                .filter(|pair| pair[0].left <= 0.0 && pair[1].left > 0.0)
                .count();
            let measured = crossings as f64 * 48_000.0 / 20_000.0;
            assert!((measured - 440.0).abs() < 35.0, "measured pitch {measured}");
        }
    }

    #[test]
    fn short_input_is_faded_out_instead_of_ending_with_a_hard_cut() {
        let output = stretch(440.0, 1.0, 200);
        assert!(!output.is_empty());
        assert!(output.last().unwrap().left.abs() < 1.0e-3);
    }

    #[test]
    fn pitch_lock_mode_changes_use_a_ten_millisecond_ramp() {
        let mut transition = PitchLockTransition::new(1_000, false);
        let wsola = StereoFrame {
            left: 1.0,
            right: 1.0,
        };
        let varispeed = StereoFrame::default();
        for _ in 0..9 {
            let _ = transition.mix(wsola, varispeed, true);
        }
        assert!(transition.weight() < 1.0);
        let _ = transition.mix(wsola, varispeed, true);
        assert_eq!(transition.weight(), 1.0);

        for _ in 0..9 {
            let _ = transition.mix(wsola, varispeed, false);
        }
        assert!(transition.weight() > 0.0);
        let _ = transition.mix(wsola, varispeed, false);
        assert_eq!(transition.weight(), 0.0);
    }
}
