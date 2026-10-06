//! Streaming EBU R 128 / ITU-R BS.1770 loudness and true-peak measurement.
//!
//! The analyzer consumes the decoded stereo stream before resampling or PCM conversion, so
//! inter-sample peaks are measured at the source rate and imported files are never buffered in full.
//! Integrated loudness uses the BS.1770 K-weighting
//! filters and 400 ms blocks with 75% overlap and the absolute and relative gates. True peak is
//! estimated with four-times windowed-sinc oversampling.

use std::collections::VecDeque;

const LOUDNESS_OFFSET_LU: f64 = -0.691;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;
pub(crate) const TRUE_PEAK_OVERSAMPLE: usize = 4;
pub(crate) const TRUE_PEAK_TAPS: usize = 16;
const TRUE_PEAK_CENTER: usize = TRUE_PEAK_TAPS / 2 - 1;
const TRUE_PEAK_FLUSH_SAMPLES: usize = TRUE_PEAK_TAPS / 2;
const TARGET_LUFS: f64 = -14.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LoudnessAnalysis {
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    /// Positive values indicate recommended gain-up; negative values indicate gain reduction.
    pub gain_offset_db: Option<f64>,
}

/// Stateful streaming analyzer. Call `push_stereo` for every sample frame and `finish` once.
pub(crate) struct LoudnessAnalyzer {
    left_shelf: Biquad,
    right_shelf: Biquad,
    left_high_pass: Biquad,
    right_high_pass: Biquad,
    block_size: usize,
    block_hop: usize,
    weighted_squares: VecDeque<[f64; 2]>,
    block_square_sum: [f64; 2],
    block_energies: Vec<f64>,
    frame_count: u64,
    true_peak: TruePeakMeter,
}

impl LoudnessAnalyzer {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate.max(1);
        let rate = f64::from(sample_rate);
        let block_size = (rate * 0.400).round().max(1.0) as usize;
        let block_hop = (rate * 0.100).round().max(1.0) as usize;
        Self {
            left_shelf: Biquad::k_weighting_shelf(rate),
            right_shelf: Biquad::k_weighting_shelf(rate),
            left_high_pass: Biquad::k_weighting_high_pass(rate),
            right_high_pass: Biquad::k_weighting_high_pass(rate),
            block_size,
            block_hop,
            weighted_squares: VecDeque::with_capacity(block_size),
            block_square_sum: [0.0; 2],
            block_energies: Vec::new(),
            frame_count: 0,
            true_peak: TruePeakMeter::new(),
        }
    }

    pub(crate) fn push_stereo(&mut self, left: f32, right: f32) {
        // Preserve valid floating-point over-range decoder samples so positive dBTP values are
        // measurable; replace only non-finite decoder output with silence.
        let left = finite_sample(left);
        let right = finite_sample(right);
        let weighted_left = self
            .left_high_pass
            .process(self.left_shelf.process(f64::from(left)));
        let weighted_right = self
            .right_high_pass
            .process(self.right_shelf.process(f64::from(right)));
        let squares = [
            weighted_left * weighted_left,
            weighted_right * weighted_right,
        ];

        self.weighted_squares.push_back(squares);
        self.block_square_sum[0] += squares[0];
        self.block_square_sum[1] += squares[1];
        if self.weighted_squares.len() > self.block_size {
            if let Some(removed) = self.weighted_squares.pop_front() {
                self.block_square_sum[0] -= removed[0];
                self.block_square_sum[1] -= removed[1];
            }
        }

        self.frame_count = self.frame_count.saturating_add(1);
        if self.weighted_squares.len() == self.block_size
            && (self.frame_count - self.block_size as u64) % self.block_hop as u64 == 0
        {
            self.record_current_block();
        }

        self.true_peak.push([f64::from(left), f64::from(right)]);
    }

    pub(crate) fn finish(mut self) -> LoudnessAnalysis {
        // A trailing incomplete 400 ms gating block is intentionally discarded, as required by
        // EBU Mode. True peak is independent of integrated-loudness block duration.
        self.true_peak.flush();

        let integrated_lufs = integrated_loudness(&self.block_energies);
        let true_peak_dbtp = self.true_peak.peak_dbtp();
        let gain_offset_db = integrated_lufs.map(|lufs| TARGET_LUFS - lufs);
        LoudnessAnalysis {
            integrated_lufs,
            true_peak_dbtp,
            gain_offset_db,
        }
    }

    fn record_current_block(&mut self) {
        let frames = self.weighted_squares.len().max(1) as f64;
        let energy = (self.block_square_sum[0] + self.block_square_sum[1]) / frames;
        if energy.is_finite() && energy > 0.0 {
            self.block_energies.push(energy);
        }
    }
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

fn integrated_loudness(block_energies: &[f64]) -> Option<f64> {
    let absolute_gate_energy = energy_for_lufs(ABSOLUTE_GATE_LUFS);
    let absolute_gated = block_energies
        .iter()
        .copied()
        .filter(|energy| energy.is_finite() && *energy > 0.0 && *energy >= absolute_gate_energy)
        .collect::<Vec<_>>();
    if absolute_gated.is_empty() {
        return None;
    }

    let preliminary_lufs = lufs_for_energy(mean(&absolute_gated));
    let relative_gate_energy = energy_for_lufs(preliminary_lufs - RELATIVE_GATE_LU);
    let gated = absolute_gated
        .into_iter()
        .filter(|energy| *energy >= relative_gate_energy)
        .collect::<Vec<_>>();
    if gated.is_empty() {
        return None;
    }
    let value = lufs_for_energy(mean(&gated));
    value.is_finite().then_some(value)
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len().max(1) as f64
}

fn energy_for_lufs(lufs: f64) -> f64 {
    10.0_f64.powf((lufs - LOUDNESS_OFFSET_LU) / 10.0)
}

fn lufs_for_energy(energy: f64) -> f64 {
    LOUDNESS_OFFSET_LU + 10.0 * energy.max(f64::MIN_POSITIVE).log10()
}

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(coefficients: [f64; 5]) -> Self {
        Self {
            b0: coefficients[0],
            b1: coefficients[1],
            b2: coefficients[2],
            a1: coefficients[3],
            a2: coefficients[4],
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// EBU/BS.1770 pre-filter coefficients generated with the specified analog prototype and
    /// bilinear transform. These equations are also valid at 44.1 kHz, not just 48 kHz.
    fn k_weighting_shelf(sample_rate: f64) -> Self {
        const FREQUENCY_HZ: f64 = 1_681.974_450_955_533;
        const GAIN_DB: f64 = 3.999_843_853_973_347;
        const Q: f64 = 0.707_175_236_955_419_6;
        const VB_EXPONENT: f64 = 0.499_666_774_154_541_6;

        let k = (std::f64::consts::PI * FREQUENCY_HZ / sample_rate).tan();
        let vh = 10.0_f64.powf(GAIN_DB / 20.0);
        let vb = vh.powf(VB_EXPONENT);
        let a0 = 1.0 + k / Q + k * k;
        Self::new([
            (vh + vb * k / Q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / Q + k * k) / a0,
            2.0 * (k * k - 1.0) / a0,
            (1.0 - k / Q + k * k) / a0,
        ])
    }

    fn k_weighting_high_pass(sample_rate: f64) -> Self {
        const FREQUENCY_HZ: f64 = 38.135_470_876_024_44;
        const Q: f64 = 0.500_327_037_323_877_3;

        let k = (std::f64::consts::PI * FREQUENCY_HZ / sample_rate).tan();
        let a0 = 1.0 + k / Q + k * k;
        Self::new([
            1.0 / a0,
            -2.0 / a0,
            1.0 / a0,
            2.0 * (k * k - 1.0) / a0,
            (1.0 - k / Q + k * k) / a0,
        ])
    }

    #[inline]
    fn process(&mut self, input: f64) -> f64 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

struct TruePeakMeter {
    history: VecDeque<[f64; 2]>,
    coefficients: [[f64; TRUE_PEAK_TAPS]; TRUE_PEAK_OVERSAMPLE],
    peak: f64,
}

impl TruePeakMeter {
    fn new() -> Self {
        let mut history = VecDeque::with_capacity(TRUE_PEAK_TAPS);
        // Pad the signal's leading edge so the first original sample is also interpolated.
        for _ in 0..TRUE_PEAK_CENTER {
            history.push_back([0.0; 2]);
        }
        Self {
            history,
            coefficients: true_peak_interpolation_coefficients(),
            peak: 0.0,
        }
    }

    fn push(&mut self, frame: [f64; 2]) {
        if self.history.len() == TRUE_PEAK_TAPS {
            self.history.pop_front();
        }
        self.history.push_back(frame);
        self.peak = self.peak.max(frame[0].abs()).max(frame[1].abs());

        if self.history.len() == TRUE_PEAK_TAPS {
            for phase in 1..TRUE_PEAK_OVERSAMPLE {
                let mut interpolated = [0.0_f64; 2];
                for (index, sample) in self.history.iter().enumerate() {
                    let coefficient = self.coefficients[phase][index];
                    interpolated[0] += sample[0] * coefficient;
                    interpolated[1] += sample[1] * coefficient;
                }
                self.peak = self
                    .peak
                    .max(interpolated[0].abs())
                    .max(interpolated[1].abs());
            }
        }
    }

    fn flush(&mut self) {
        for _ in 0..TRUE_PEAK_FLUSH_SAMPLES {
            self.push([0.0; 2]);
        }
    }

    fn peak_dbtp(&self) -> Option<f64> {
        (self.peak > 0.0 && self.peak.is_finite()).then(|| 20.0 * self.peak.log10())
    }
}

pub(crate) fn true_peak_interpolation_coefficients() -> [[f64; TRUE_PEAK_TAPS]; TRUE_PEAK_OVERSAMPLE]
{
    let mut coefficients = [[0.0; TRUE_PEAK_TAPS]; TRUE_PEAK_OVERSAMPLE];
    let half_width = TRUE_PEAK_TAPS as f64 / 2.0;
    for (phase, phase_coefficients) in coefficients.iter_mut().enumerate() {
        let fraction = phase as f64 / TRUE_PEAK_OVERSAMPLE as f64;
        let target = TRUE_PEAK_CENTER as f64 + fraction;
        let mut total = 0.0;
        for (index, coefficient) in phase_coefficients.iter_mut().enumerate() {
            let distance = index as f64 - target;
            let absolute_distance = distance.abs();
            if absolute_distance < half_width {
                let sinc = if absolute_distance < 1.0e-12 {
                    1.0
                } else {
                    let angle = std::f64::consts::PI * distance;
                    angle.sin() / angle
                };
                let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / half_width).cos();
                *coefficient = sinc * window;
                total += *coefficient;
            }
        }
        if total.abs() > 1.0e-12 {
            for coefficient in phase_coefficients {
                *coefficient /= total;
            }
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::LoudnessAnalyzer;

    fn measure_tone(
        amplitude: f32,
        frequency_hz: f64,
        sample_rate: u32,
        seconds: f64,
    ) -> super::LoudnessAnalysis {
        let mut analyzer = LoudnessAnalyzer::new(sample_rate);
        let frame_count = (f64::from(sample_rate) * seconds) as usize;
        for index in 0..frame_count {
            let phase =
                std::f64::consts::TAU * frequency_hz * index as f64 / f64::from(sample_rate);
            let sample = amplitude * phase.sin() as f32;
            analyzer.push_stereo(sample, sample);
        }
        analyzer.finish()
    }

    #[test]
    fn dual_mono_sine_matches_bs1770_loudness_and_target_gain() {
        let analysis = measure_tone(0.1, 1_000.0, 48_000, 4.0);
        let lufs = analysis.integrated_lufs.unwrap();
        // The BS.1770 K-weighting transfer magnitude at 1 kHz is about +0.654 dB.
        let expected_lufs = -0.691 + 20.0 * 0.1_f64.log10() + 0.654;
        assert!(
            (lufs - expected_lufs).abs() < 0.12,
            "measured {lufs}, expected {expected_lufs}"
        );
        assert!((analysis.gain_offset_db.unwrap() - (-14.0 - lufs)).abs() < 1.0e-9);

        let analysis_44k = measure_tone(0.1, 1_000.0, 44_100, 4.0);
        let lufs_44k = analysis_44k.integrated_lufs.unwrap();
        let expected_44k = -0.691 + 20.0 * 0.1_f64.log10() + 0.653;
        assert!(
            (lufs_44k - expected_44k).abs() < 0.12,
            "measured {lufs_44k}, expected {expected_44k}"
        );
    }

    #[test]
    fn true_peak_reports_inter_sample_peak_and_not_only_sample_peak() {
        let analysis = measure_tone(0.8, 7_321.0, 48_000, 2.0);
        let true_peak = analysis.true_peak_dbtp.unwrap();
        let expected_dbtp = 20.0 * 0.8_f64.log10();
        assert!(
            true_peak >= expected_dbtp - 0.08,
            "true peak was {true_peak} dBTP"
        );
        assert!(
            true_peak <= expected_dbtp + 0.08,
            "true peak was {true_peak} dBTP"
        );

        let over_range = measure_tone(1.05, 997.0, 48_000, 2.0)
            .true_peak_dbtp
            .unwrap();
        let expected_over_range = 20.0 * 1.05_f64.log10();
        assert!((over_range - expected_over_range).abs() < 0.08);
        assert!(over_range > 0.0, "positive true peaks must not be clipped");
    }

    #[test]
    fn digital_silence_and_incomplete_blocks_are_gated() {
        let mut silence = LoudnessAnalyzer::new(44_100);
        for _ in 0..44_100 {
            silence.push_stereo(0.0, 0.0);
        }
        let silence = silence.finish();
        assert_eq!(silence.integrated_lufs, None);
        assert_eq!(silence.true_peak_dbtp, None);
        assert_eq!(silence.gain_offset_db, None);

        let short = measure_tone(0.25, 440.0, 44_100, 0.1);
        assert_eq!(short.integrated_lufs, None);
        assert!(short.true_peak_dbtp.is_some());
    }
}
