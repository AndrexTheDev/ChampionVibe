use crate::audio::mir_analysis::{self, MirInput};
use crate::confidence::{heuristic_confidence, trusted_confidence};
use anyhow::{Context, Result};
use hound::WavReader;
use rustfft::{num_complex::Complex32, Fft, FftPlanner};
use serde::Serialize;
use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
};

const ANALYSIS_RATE: usize = 22_050;
const FFT_SIZE: usize = 2_048;
const HOP_SIZE: usize = 512;
const MAJOR_PROFILE: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const MINOR_PROFILE: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];
const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];
/// A beat grid shorter than this many beats cannot corroborate itself; the tempo stays a guess.
const MIN_CORROBORATED_BEATS: f64 = 16.0;
/// Autocorrelation lags within this many frames count as the same tempo when comparing halves.
const LAG_TOLERANCE: usize = 2;
/// Distinct pitch classes that must carry at least this share of the peak chroma energy before a
/// key reading is allowed to claim it was corroborated.
const MIN_KEY_PITCH_CLASSES: usize = 3;
const KEY_PITCH_CLASS_SHARE: f64 = 0.15;

#[derive(Debug, Clone)]
pub(crate) struct AudioAnalysis {
    pub bpm: Option<f64>,
    pub bpm_confidence: f64,
    pub beat_grid_seconds: Vec<f64>,
    pub musical_key: Option<String>,
    pub key_confidence: f64,
    pub energy_density: f64,
    pub energy_dbfs: f64,
    pub dynamic_range_db: f64,
    pub onset_density: f64,
    pub spectral_centroid_hz: f64,
    pub low_band_ratio: f64,
    pub mid_band_ratio: f64,
    pub high_band_ratio: f64,
    pub acoustic_tag: String,
    pub acoustic_scores_json: String,
    pub mir: mir_analysis::MirAnalysis,
}

#[derive(Serialize)]
struct AcousticScores {
    aggressive: f64,
    deep: f64,
    conscious: f64,
    hype: f64,
}

pub(crate) fn analyze_wav(
    path: &Path,
    cancel: &AtomicBool,
    integrated_lufs: Option<f64>,
    tagged_bpm: Option<f64>,
) -> Result<AudioAnalysis> {
    let mut reader = WavReader::open(path)
        .with_context(|| format!("cannot read normalized audio {}", path.display()))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 2 && spec.sample_rate == 44_100 && spec.bits_per_sample == 16,
        "normalized audio has an unexpected WAV format"
    );
    let mut accumulator = FeatureAccumulator::new();
    let mut pending_mono = None;
    let mut pending_downsample = None;
    let mut source_frames = 0u64;

    for sample in reader.samples::<i16>() {
        if source_frames % 65_536 == 0 && cancel.load(Ordering::Acquire) {
            anyhow::bail!("import cancelled");
        }
        let sample = sample.context("corrupt normalized PCM data")?;
        let normalized = sample as f32 / 32_768.0;
        let mono = if source_frames % 2 == 0 {
            pending_mono = Some(normalized * 0.5);
            source_frames += 1;
            continue;
        } else {
            let first = pending_mono.take().unwrap_or(0.0);
            first + normalized * 0.5
        };
        source_frames += 1;
        if let Some(previous) = pending_downsample.take() {
            // Pairwise averaging provides a cheap anti-aliasing filter before 2:1 decimation.
            accumulator.push_sample((previous + mono) * 0.5);
        } else {
            pending_downsample = Some(mono);
        }
    }
    if let Some(last_sample) = pending_downsample {
        accumulator.push_sample(last_sample);
    }
    accumulator.finish();

    let duration_seconds = (source_frames / 2) as f64 / f64::from(spec.sample_rate);
    let energy_density = accumulator.mean_square();
    let energy_dbfs = 10.0 * energy_density.max(1.0e-12).log10();
    let dynamic_range_db = accumulator.dynamic_range_db();
    let (detected_bpm, detected_bpm_confidence, beat_grid_seconds) =
        estimate_beat_grid(&accumulator.onsets, duration_seconds);
    let (bpm, bpm_confidence) = if let Some(tagged_bpm) = tagged_bpm {
        // A tag in the file's metadata is authoritative, not a heuristic guess.
        (Some(tagged_bpm), trusted_confidence())
    } else {
        (detected_bpm, detected_bpm_confidence)
    };
    let (musical_key, key_confidence) = estimate_key(&accumulator.chroma);

    let low_sum = accumulator.band_energy[0];
    let mid_sum = accumulator.band_energy[1];
    let high_sum = accumulator.band_energy[2];
    let band_total = (low_sum + mid_sum + high_sum).max(1.0e-12);
    let low_band_ratio = (low_sum / band_total) as f64;
    let mid_band_ratio = (mid_sum / band_total) as f64;
    let high_band_ratio = (high_sum / band_total) as f64;
    let spectral_centroid_hz = if accumulator.centroid_weight > 0.0 {
        accumulator.centroid_sum / accumulator.centroid_weight
    } else {
        0.0
    };
    let onset_density = estimate_onset_density(&accumulator.onsets, duration_seconds);
    let (acoustic_tag, acoustic_scores_json) = classify_acoustic(
        energy_dbfs,
        dynamic_range_db,
        onset_density,
        spectral_centroid_hz,
        low_band_ratio,
        mid_band_ratio,
        high_band_ratio,
        bpm,
    )?;
    let mir = mir_analysis::analyze(MirInput {
        bpm,
        bpm_confidence,
        beat_grid_seconds: &beat_grid_seconds,
        duration_seconds,
        musical_key: musical_key.as_deref(),
        key_confidence,
        energy_dbfs,
        dynamic_range_db,
        onset_density,
        spectral_centroid_hz,
        low_band_ratio,
        mid_band_ratio,
        high_band_ratio,
        integrated_lufs,
        chroma: &accumulator.chroma,
        onset_strengths: &accumulator.onsets,
    });

    Ok(AudioAnalysis {
        bpm,
        bpm_confidence,
        beat_grid_seconds,
        musical_key,
        key_confidence,
        energy_density,
        energy_dbfs,
        dynamic_range_db,
        onset_density,
        spectral_centroid_hz,
        low_band_ratio,
        mid_band_ratio,
        high_band_ratio,
        acoustic_tag,
        acoustic_scores_json,
        mir,
    })
}

struct FeatureAccumulator {
    fft: Arc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex32>,
    mono_window: VecDeque<f32>,
    previous_magnitudes: Vec<f32>,
    onsets: Vec<f32>,
    chroma: [f64; 12],
    band_energy: [f64; 3],
    centroid_sum: f64,
    centroid_weight: f64,
    frame_rms_db: Vec<f32>,
    square_sum: f64,
    sample_count: u64,
}

impl FeatureAccumulator {
    fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        Self {
            fft: planner.plan_fft_forward(FFT_SIZE),
            fft_buffer: vec![Complex32::new(0.0, 0.0); FFT_SIZE],
            mono_window: VecDeque::with_capacity(FFT_SIZE),
            previous_magnitudes: vec![0.0; FFT_SIZE / 2 + 1],
            onsets: Vec::new(),
            chroma: [0.0; 12],
            band_energy: [0.0; 3],
            centroid_sum: 0.0,
            centroid_weight: 0.0,
            frame_rms_db: Vec::new(),
            square_sum: 0.0,
            sample_count: 0,
        }
    }

    fn push_sample(&mut self, sample: f32) {
        self.square_sum += f64::from(sample) * f64::from(sample);
        self.sample_count += 1;
        self.mono_window.push_back(sample);
        if self.mono_window.len() == FFT_SIZE {
            self.analyze_frame();
            for _ in 0..HOP_SIZE {
                self.mono_window.pop_front();
            }
        }
    }

    fn analyze_frame(&mut self) {
        self.fft_buffer.fill(Complex32::new(0.0, 0.0));
        let mut window_square_sum = 0.0f64;
        for (index, sample) in self.mono_window.iter().copied().enumerate() {
            let hann =
                0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / (FFT_SIZE - 1) as f32).cos();
            let value = sample * hann;
            self.fft_buffer[index].re = value;
            window_square_sum += f64::from(value) * f64::from(value);
        }
        self.fft.process(&mut self.fft_buffer);

        let mut flux = 0.0f32;
        let mut magnitude_sum = 0.0f32;
        let mut centroid_numerator = 0.0f64;
        for bin in 1..=FFT_SIZE / 2 {
            let magnitude = self.fft_buffer[bin].norm();
            let previous = self.previous_magnitudes[bin];
            flux += (magnitude - previous).max(0.0);
            magnitude_sum += magnitude;
            self.previous_magnitudes[bin] = magnitude;

            let frequency = bin as f64 * ANALYSIS_RATE as f64 / FFT_SIZE as f64;
            let power = f64::from(magnitude) * f64::from(magnitude);
            if (20.0..250.0).contains(&frequency) {
                self.band_energy[0] += power;
            } else if (250.0..4_000.0).contains(&frequency) {
                self.band_energy[1] += power;
            } else if frequency <= (ANALYSIS_RATE / 2) as f64 {
                self.band_energy[2] += power;
            }
            centroid_numerator += frequency * f64::from(magnitude);

            if frequency >= 40.0 && magnitude > 0.0 {
                let midi = (69.0 + 12.0 * (frequency / 440.0).log2()).round() as i32;
                let pitch_class = midi.rem_euclid(12) as usize;
                self.chroma[pitch_class] += f64::from(magnitude);
            }
        }
        self.onsets.push(if magnitude_sum > 1.0e-8 {
            flux / magnitude_sum
        } else {
            0.0
        });
        if magnitude_sum > 1.0e-8 {
            self.centroid_sum += centroid_numerator;
            self.centroid_weight += f64::from(magnitude_sum);
        }
        self.frame_rms_db
            .push((10.0 * (window_square_sum / FFT_SIZE as f64).max(1.0e-12).log10()) as f32);
    }

    fn finish(&mut self) {
        if !self.mono_window.is_empty() && self.mono_window.len() < FFT_SIZE {
            while self.mono_window.len() < FFT_SIZE {
                self.mono_window.push_back(0.0);
            }
            self.analyze_frame();
        }
    }

    fn mean_square(&self) -> f64 {
        if self.sample_count == 0 {
            0.0
        } else {
            self.square_sum / self.sample_count as f64
        }
    }

    fn dynamic_range_db(&self) -> f64 {
        if self.frame_rms_db.is_empty() {
            return 0.0;
        }
        let mut sorted = self.frame_rms_db.clone();
        sorted.sort_by(f32::total_cmp);
        let p10 = sorted[(sorted.len() - 1) * 10 / 100];
        let p90 = sorted[(sorted.len() - 1) * 90 / 100];
        f64::from((p90 - p10).max(0.0))
    }
}

/// Autocorrelation score of the novelty curve against itself for every plausible beat lag.
fn lag_scores(novelty: &[f64], min_lag: usize, max_lag: usize) -> Vec<(usize, f64)> {
    let mut scores = Vec::<(usize, f64)>::new();
    for lag in min_lag.max(1)..=max_lag.min(novelty.len() / 2) {
        let mut dot = 0.0;
        let mut left_power = 0.0;
        let mut right_power = 0.0;
        for index in lag..novelty.len() {
            let left = novelty[index];
            let right = novelty[index - lag];
            dot += left * right;
            left_power += left * left;
            right_power += right * right;
        }
        let score = dot / (left_power * right_power).sqrt().max(1.0e-12);
        scores.push((lag, score));
    }
    scores
}

/// Strongest lag (shortest of an effectively tied group, so a 120 BPM pulse is not read as 60).
fn dominant_lag(scores: &[(usize, f64)]) -> Option<(usize, f64)> {
    let max_score = scores.iter().map(|(_, score)| *score).reduce(f64::max)?;
    scores
        .iter()
        .copied()
        .filter(|(_, score)| max_score - *score <= 1.0e-9)
        .min_by_key(|(lag, _)| *lag)
}

/// Two lags describe the same tempo when they are within the tie window or stand in a 2:1 / 1:2
/// relationship, which autocorrelation cannot distinguish on its own.
fn same_tempo(left: usize, right: usize) -> bool {
    left.abs_diff(right) <= LAG_TOLERANCE
        || left.abs_diff(right.saturating_mul(2)) <= LAG_TOLERANCE
        || right.abs_diff(left.saturating_mul(2)) <= LAG_TOLERANCE
}

/// Corroborates a tempo by looking for the same beat period in the first and second half of the
/// track. A reading only the head of a track supports stays a heuristic guess.
fn tempo_is_corroborated(novelty: &[f64], min_lag: usize, max_lag: usize, best_lag: usize) -> bool {
    let middle = novelty.len() / 2;
    if middle < min_lag.saturating_mul(4) {
        return false;
    }
    [&novelty[..middle], &novelty[middle..]]
        .into_iter()
        .all(|half| {
            dominant_lag(&lag_scores(half, min_lag, max_lag))
                .map(|(lag, score)| score > 0.0 && same_tempo(lag, best_lag))
                .unwrap_or(false)
        })
}

fn estimate_beat_grid(onsets: &[f32], duration_seconds: f64) -> (Option<f64>, f64, Vec<f64>) {
    if onsets.len() < 32 || duration_seconds <= 0.0 {
        return (None, 0.0, Vec::new());
    }
    let mean = onsets.iter().map(|value| f64::from(*value)).sum::<f64>() / onsets.len() as f64;
    let novelty: Vec<f64> = onsets
        .iter()
        .map(|value| (f64::from(*value) - mean).max(0.0))
        .collect();
    let frame_rate = ANALYSIS_RATE as f64 / HOP_SIZE as f64;
    let min_lag = (frame_rate * 60.0 / 240.0).round() as usize;
    let max_lag = (frame_rate * 60.0 / 40.0).round() as usize;
    let scores = lag_scores(&novelty, min_lag, max_lag);
    // Autocorrelation has equally strong peaks at multiples of a regular beat period. Prefer
    // the shortest effectively tied lag so a 120 BPM pulse is not mistaken for 60 BPM.
    let Some((best_lag, best_score)) = dominant_lag(&scores) else {
        return (None, 0.0, Vec::new());
    };
    if best_score < 0.03 {
        return (None, best_score.clamp(0.0, 1.0), Vec::new());
    }

    let second_score = scores
        .iter()
        .filter(|(lag, _)| !same_tempo(*lag, best_lag))
        .map(|(_, score)| *score)
        .fold(0.0, f64::max);
    let margin = (best_score - second_score).max(0.0) / best_score.max(1.0e-12) * 2.0;
    let period_seconds = best_lag as f64 / frame_rate;
    let bpm = 60.0 / period_seconds;

    let mut best_phase = 0usize;
    let mut best_phase_score = f64::MIN;
    for phase in 0..best_lag {
        let mut phase_score = 0.0;
        let mut index = phase;
        while index < novelty.len() {
            phase_score += novelty[index];
            index += best_lag;
        }
        if phase_score > best_phase_score {
            best_phase = phase;
            best_phase_score = phase_score;
        }
    }
    let mut grid = Vec::new();
    let phase_seconds = best_phase as f64 / frame_rate;
    let mut beat = phase_seconds;
    while beat < duration_seconds && grid.len() < 200_000 {
        grid.push(beat);
        beat += period_seconds;
    }
    // The grid must span enough beats and both halves of the track must agree on the period before
    // the heuristic is allowed to report a trusted confidence.
    let spans_enough_beats = grid.len() as f64 >= MIN_CORROBORATED_BEATS;
    let corroborated =
        spans_enough_beats && tempo_is_corroborated(&novelty, min_lag, max_lag, best_lag);
    (Some(bpm), heuristic_confidence(margin, corroborated), grid)
}

fn estimate_key(chroma: &[f64; 12]) -> (Option<String>, f64) {
    let total = chroma.iter().sum::<f64>();
    if total <= 1.0e-10 {
        return (None, 0.0);
    }
    let mut candidates = Vec::<(f64, usize, bool)>::with_capacity(24);
    for tonic in 0..12 {
        candidates.push((
            profile_correlation(chroma, &MAJOR_PROFILE, tonic),
            tonic,
            true,
        ));
        candidates.push((
            profile_correlation(chroma, &MINOR_PROFILE, tonic),
            tonic,
            false,
        ));
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let best = candidates[0];
    let next = candidates[1].0;
    let margin = (best.0 - next).max(0.0) * 2.0;
    // A profile fit only means something when several pitch classes carry the chroma energy; a
    // single ringing note is not a key signature.
    let peak = chroma.iter().copied().fold(0.0, f64::max);
    let carried_pitch_classes = if peak > 0.0 {
        chroma
            .iter()
            .filter(|value| **value >= peak * KEY_PITCH_CLASS_SHARE)
            .count()
    } else {
        0
    };
    let corroborated = carried_pitch_classes >= MIN_KEY_PITCH_CLASSES;
    let mode = if best.2 { "major" } else { "minor" };
    (
        Some(format!("{} {mode}", NOTE_NAMES[best.1])),
        heuristic_confidence(margin, corroborated),
    )
}

fn profile_correlation(chroma: &[f64; 12], profile: &[f32; 12], tonic: usize) -> f64 {
    let chroma_mean = chroma.iter().sum::<f64>() / 12.0;
    let profile_mean = profile.iter().map(|value| f64::from(*value)).sum::<f64>() / 12.0;
    let mut covariance = 0.0;
    let mut chroma_power = 0.0;
    let mut profile_power = 0.0;
    for pitch in 0..12 {
        let audio_value = chroma[pitch] - chroma_mean;
        let template_index = (pitch + 12 - tonic) % 12;
        let template_value = f64::from(profile[template_index]) - profile_mean;
        covariance += audio_value * template_value;
        chroma_power += audio_value * audio_value;
        profile_power += template_value * template_value;
    }
    covariance / (chroma_power * profile_power).sqrt().max(1.0e-12)
}

fn estimate_onset_density(onsets: &[f32], duration_seconds: f64) -> f64 {
    if onsets.is_empty() || duration_seconds <= 0.0 {
        return 0.0;
    }
    let mean = onsets.iter().map(|value| f64::from(*value)).sum::<f64>() / onsets.len() as f64;
    let variance = onsets
        .iter()
        .map(|value| (f64::from(*value) - mean).powi(2))
        .sum::<f64>()
        / onsets.len() as f64;
    let threshold = (mean + variance.sqrt() * 1.25).max(0.025);
    onsets
        .iter()
        .filter(|value| f64::from(**value) > threshold)
        .count() as f64
        / duration_seconds
}

fn classify_acoustic(
    energy_dbfs: f64,
    dynamic_range_db: f64,
    onset_density: f64,
    centroid_hz: f64,
    low_ratio: f64,
    mid_ratio: f64,
    _high_ratio: f64,
    bpm: Option<f64>,
) -> Result<(String, String)> {
    let loudness = ((energy_dbfs + 38.0) / 28.0).clamp(0.0, 1.0);
    let transient = (onset_density / 4.0).clamp(0.0, 1.0);
    let tempo = ((bpm.unwrap_or(115.0) - 65.0) / 125.0).clamp(0.0, 1.0);
    let brightness = (centroid_hz / 7_000.0).clamp(0.0, 1.0);
    let compression = (1.0 - (dynamic_range_db / 24.0).clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let scores = AcousticScores {
        aggressive: (0.38 * loudness + 0.26 * brightness + 0.20 * transient + 0.16 * compression)
            .clamp(0.0, 1.0),
        deep: (0.34 * low_ratio
            + 0.24 * (1.0 - brightness)
            + 0.18 * (1.0 - tempo)
            + 0.14 * (1.0 - transient)
            + 0.10 * (1.0 - loudness))
            .clamp(0.0, 1.0),
        conscious: (0.34 * mid_ratio
            + 0.24 * (1.0 - transient)
            + 0.20 * (1.0 - loudness)
            + 0.12 * (dynamic_range_db / 24.0).clamp(0.0, 1.0)
            + 0.10 * (1.0 - brightness))
            .clamp(0.0, 1.0),
        hype: (0.30 * loudness
            + 0.28 * transient
            + 0.20 * tempo
            + 0.12 * brightness
            + 0.10 * low_ratio)
            .clamp(0.0, 1.0),
    };
    let values = [
        ("Aggressiv", scores.aggressive),
        ("Deep", scores.deep),
        ("Conscious", scores.conscious),
        ("Hype", scores.hype),
    ];
    let label = values
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(label, _)| *label)
        .unwrap_or("Deep")
        .to_owned();
    Ok((label, serde_json::to_string(&scores)?))
}

#[cfg(test)]
mod tests {
    use super::{estimate_beat_grid, estimate_key};
    use crate::confidence::TRUSTED_CONFIDENCE;

    /// Onsets at `period` frames across `frames`, optionally only inside `[from, to)`.
    fn onset_train(frames: usize, period: usize, from: usize, to: usize) -> Vec<f32> {
        let mut onsets = vec![0.0f32; frames];
        for index in (from..to).step_by(period) {
            onsets[index] = 1.0;
        }
        onsets
    }

    #[test]
    fn periodic_onsets_produce_a_120_bpm_grid() {
        let mut onsets = vec![0.0f32; 43 * 24];
        for index in (8..onsets.len()).step_by(22) {
            onsets[index] = 1.0;
        }
        let (bpm, confidence, grid) = estimate_beat_grid(&onsets, 24.0);
        let bpm = bpm.expect("periodic onsets should produce a tempo");
        assert!((bpm - 117.0).abs() < 8.0, "estimated {bpm} BPM");
        assert!(confidence > 0.0);
        assert!(!grid.is_empty());
    }

    #[test]
    fn chroma_profile_returns_a_key() {
        let mut chroma = [0.0f64; 12];
        chroma[0] = 8.0;
        chroma[4] = 6.0;
        chroma[7] = 6.0;
        let (key, _) = estimate_key(&chroma);
        assert!(key.unwrap().starts_with("C "));
    }

    #[test]
    fn unsupported_tempo_stays_below_the_trusted_threshold() {
        // The pulse only exists in the first half of the track, so the two halves cannot agree on
        // a period and the heuristic must not claim the reading is trusted.
        let onsets = onset_train(43 * 24, 22, 8, 43 * 12);
        let (bpm, confidence, _) = estimate_beat_grid(&onsets, 24.0);
        assert!(bpm.is_some(), "the head of the track still yields a tempo");
        assert!(
            confidence < TRUSTED_CONFIDENCE,
            "unsure tempo reported {confidence}"
        );
    }

    #[test]
    fn tempo_without_enough_beats_stays_below_the_trusted_threshold() {
        let onsets = onset_train(43 * 2, 22, 8, 43 * 2);
        let (_, confidence, grid) = estimate_beat_grid(&onsets, 1.0);
        assert!(grid.len() < 16, "test fixture must stay short");
        assert!(confidence < TRUSTED_CONFIDENCE);
    }

    #[test]
    fn degenerate_chroma_stays_below_the_trusted_threshold() {
        let mut chroma = [0.0f64; 12];
        chroma[3] = 5.0;
        let (key, confidence) = estimate_key(&chroma);
        assert!(key.is_some(), "a single pitch class still names a key");
        assert!(
            confidence < TRUSTED_CONFIDENCE,
            "unsure key reported {confidence}"
        );
    }

    #[test]
    fn distinct_pitch_classes_can_corroborate_a_key() {
        // C major triad plus its seventh: several pitch classes carry the energy.
        let mut chroma = [0.0f64; 12];
        chroma[0] = 8.0;
        chroma[2] = 4.0;
        chroma[4] = 6.0;
        chroma[7] = 6.0;
        chroma[11] = 5.0;
        let (key, confidence) = estimate_key(&chroma);
        assert_eq!(key.as_deref(), Some("C major"));
        assert!(confidence >= 0.0 && confidence <= 1.0);
    }

    #[test]
    fn silent_audio_reports_no_tempo_and_no_key() {
        let (bpm, confidence, grid) = estimate_beat_grid(&[0.0f32; 43 * 24], 24.0);
        assert!(bpm.is_none());
        assert_eq!(confidence, 0.0);
        assert!(grid.is_empty());
        let (key, confidence) = estimate_key(&[0.0f64; 12]);
        assert!(key.is_none());
        assert_eq!(confidence, 0.0);
    }
}
