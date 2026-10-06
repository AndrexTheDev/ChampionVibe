//! Lightweight, local MIR descriptors used for mood, rhythm and style tagging.
//!
//! These deterministic heuristics are intentionally kept separate from learned-model output. They
//! provide useful, reproducible tags and a compact 64-byte quantized descriptor, but are not a
//! substitute for a licensed, trained ONNX audio-embedding model.

const FEATURE_VECTOR_SIZE: usize = 64;
const ANALYSIS_SAMPLE_RATE: f64 = 22_050.0;
const ANALYSIS_HOP_SIZE: f64 = 512.0;
const ONSET_FRAME_RATE: f64 = ANALYSIS_SAMPLE_RATE / ANALYSIS_HOP_SIZE;
const RHYTHM_SLOTS: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct MirAnalysis {
    pub mood_valence: f64,
    pub mood_arousal: f64,
    pub mood_label: String,
    pub rhythm_pattern: String,
    pub rhythm_confidence: f64,
    pub style_genre: String,
    /// Signed int8 descriptor bytes; it is not a learned ONNX embedding.
    pub feature_vector: [i8; FEATURE_VECTOR_SIZE],
}

pub(crate) struct MirInput<'a> {
    pub bpm: Option<f64>,
    pub bpm_confidence: f64,
    pub beat_grid_seconds: &'a [f64],
    pub duration_seconds: f64,
    pub musical_key: Option<&'a str>,
    pub key_confidence: f64,
    pub energy_dbfs: f64,
    pub dynamic_range_db: f64,
    pub onset_density: f64,
    pub spectral_centroid_hz: f64,
    pub low_band_ratio: f64,
    pub mid_band_ratio: f64,
    pub high_band_ratio: f64,
    pub integrated_lufs: Option<f64>,
    pub chroma: &'a [f64; 12],
    pub onset_strengths: &'a [f32],
}

pub(crate) fn analyze(input: MirInput<'_>) -> MirAnalysis {
    let rhythm = analyze_rhythm(
        input.onset_strengths,
        input.beat_grid_seconds,
        input.bpm,
        input.duration_seconds,
    );
    let (mood_valence, mood_arousal) = estimate_affect(&input);
    let mood_label = mood_label(mood_valence, mood_arousal).to_owned();
    let style_genre = estimate_style(&input, &rhythm.pattern).to_owned();
    let feature_vector = quantized_feature_vector(&input, &rhythm, mood_valence, mood_arousal);

    MirAnalysis {
        mood_valence,
        mood_arousal,
        mood_label,
        rhythm_pattern: rhythm.pattern,
        rhythm_confidence: rhythm.confidence,
        style_genre,
        feature_vector,
    }
}

struct RhythmAnalysis {
    pattern: String,
    confidence: f64,
    slots: [f64; RHYTHM_SLOTS],
    syncopation: f64,
}

fn analyze_rhythm(
    onset_strengths: &[f32],
    beat_grid_seconds: &[f64],
    bpm: Option<f64>,
    duration_seconds: f64,
) -> RhythmAnalysis {
    let mut slots = [0.0_f64; RHYTHM_SLOTS];
    let mut slot_counts = [0_u32; RHYTHM_SLOTS];
    let period_seconds = bpm
        .filter(|value| value.is_finite() && (40.0..=240.0).contains(value))
        .map(|value| 60.0 / value)
        .or_else(|| median_beat_period(beat_grid_seconds));
    let Some(period_seconds) = period_seconds.filter(|period| period.is_finite() && *period > 0.0) else {
        return RhythmAnalysis {
            pattern: "Unknown".to_owned(),
            confidence: 0.0,
            slots,
            syncopation: 0.0,
        };
    };
    if onset_strengths.is_empty() || !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return RhythmAnalysis {
            pattern: "Unknown".to_owned(),
            confidence: 0.0,
            slots,
            syncopation: 0.0,
        };
    }

    let mean_onset = onset_strengths
        .iter()
        .map(|value| f64::from(*value).max(0.0))
        .sum::<f64>()
        / onset_strengths.len() as f64;
    let anchor = beat_grid_seconds
        .first()
        .copied()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.0);
    let subdivision_seconds = period_seconds / 4.0;
    if !subdivision_seconds.is_finite() || subdivision_seconds <= 0.0 {
        return RhythmAnalysis {
            pattern: "Unknown".to_owned(),
            confidence: 0.0,
            slots,
            syncopation: 0.0,
        };
    }
    let slot_count = (((duration_seconds - anchor).max(0.0) / subdivision_seconds).ceil() as usize)
        .min(2_000_000);
    for position in 0..slot_count {
        let time_seconds = anchor + position as f64 * subdivision_seconds;
        if time_seconds >= duration_seconds {
            break;
        }
        let frame = (time_seconds * ONSET_FRAME_RATE).round() as isize;
        let strength = (-1..=1)
            .map(|offset| {
                let index = frame + offset;
                if index >= 0 {
                    onset_strengths
                        .get(index as usize)
                        .copied()
                        .map(f64::from)
                        .unwrap_or(0.0)
                } else {
                    0.0
                }
            })
            .fold(0.0_f64, f64::max);
        let slot = position % RHYTHM_SLOTS;
        slots[slot] += (strength - mean_onset * 0.5).max(0.0);
        slot_counts[slot] = slot_counts[slot].saturating_add(1);
    }
    for (value, count) in slots.iter_mut().zip(slot_counts) {
        if count > 0 {
            *value /= f64::from(count);
        }
    }

    let total = slots.iter().sum::<f64>();
    let peak = slots.iter().copied().fold(0.0_f64, f64::max);
    if total <= 1.0e-9 || peak <= 1.0e-9 {
        return RhythmAnalysis {
            pattern: "Unknown".to_owned(),
            confidence: 0.0,
            slots,
            syncopation: 0.0,
        };
    }

    let quarter_energy = (0..RHYTHM_SLOTS)
        .filter(|slot| slot % 4 == 0)
        .map(|slot| slots[slot])
        .sum::<f64>();
    let offbeat_energy = (0..RHYTHM_SLOTS)
        .filter(|slot| slot % 4 != 0)
        .map(|slot| slots[slot])
        .sum::<f64>();
    let syncopation = offbeat_energy / total;
    let quarter_mean = quarter_energy / (RHYTHM_SLOTS / 4) as f64;
    let offbeat_mean = offbeat_energy / (RHYTHM_SLOTS - RHYTHM_SLOTS / 4) as f64;
    let beat_one_three = (slots[0] + slots[8] + slots[16] + slots[24]) / 4.0;
    let backbeat_two_four = (slots[4] + slots[12] + slots[20] + slots[28]) / 4.0;
    let one_drop_hits = (slots[8] + slots[24]) / 2.0;
    let other_quarter_hits =
        (slots[0] + slots[4] + slots[12] + slots[16] + slots[20] + slots[28]) / 6.0;
    let quarter_min = (0..RHYTHM_SLOTS)
        .filter(|slot| slot % 4 == 0)
        .map(|slot| slots[slot])
        .fold(f64::INFINITY, f64::min);
    let beat_ratio = bpm.unwrap_or(120.0);

    let (pattern, confidence) = if beat_ratio <= 130.0
        && one_drop_hits > peak * 0.45
        && one_drop_hits > other_quarter_hits * 1.45
        && syncopation < 0.55
    {
        (
            "One Drop",
            ((one_drop_hits - other_quarter_hits) / peak.max(1.0e-9)).clamp(0.0, 1.0),
        )
    } else if beat_ratio <= 125.0
        && syncopation < 0.25
        && quarter_mean > peak * 0.25
        && quarter_min > quarter_mean * 0.35
    {
        (
            "Steppers",
            (quarter_min / quarter_mean.max(1.0e-9) * (1.0 - syncopation)).clamp(0.0, 1.0),
        )
    } else if syncopation > 0.15
        && template_coverage(&slots, &[0, 6, 12, 20, 28]) > 0.72
    {
        ("Clave", template_coverage(&slots, &[0, 6, 12, 20, 28]))
    } else if syncopation > 0.22
        && template_coverage(
        &slots,
        &[0, 4, 6, 8, 11, 12, 14, 16, 20, 22, 24, 27, 28, 30],
    ) > 0.68
        && beat_ratio >= 65.0
    {
        (
            "Dembow",
            template_coverage(
                &slots,
                &[0, 4, 6, 8, 11, 12, 14, 16, 20, 22, 24, 27, 28, 30],
            ),
        )
    } else if (65.0..=115.0).contains(&beat_ratio)
        && backbeat_two_four > beat_one_three * 1.2
        && syncopation < 0.35
    {
        (
            "Boom Bap",
            ((backbeat_two_four - beat_one_three) / peak.max(1.0e-9)).clamp(0.0, 1.0),
        )
    } else if syncopation > 0.30 && offbeat_mean > quarter_mean * 0.75 {
        ("Syncopated", syncopation.clamp(0.0, 1.0))
    } else {
        ("Straight 4/4", (1.0 - syncopation).clamp(0.0, 1.0) * 0.5)
    };

    RhythmAnalysis {
        pattern: pattern.to_owned(),
        confidence: confidence.clamp(0.0, 1.0),
        slots,
        syncopation,
    }
}

fn median_beat_period(beat_grid_seconds: &[f64]) -> Option<f64> {
    if beat_grid_seconds.len() < 2 {
        return None;
    }
    let mut periods = beat_grid_seconds
        .windows(2)
        .filter_map(|pair| {
            let period = pair[1] - pair[0];
            (period.is_finite() && period > 0.0).then_some(period)
        })
        .collect::<Vec<_>>();
    periods.sort_by(f64::total_cmp);
    periods.get(periods.len() / 2).copied()
}

fn template_coverage(slots: &[f64; RHYTHM_SLOTS], target_positions: &[usize]) -> f64 {
    let total = slots.iter().sum::<f64>();
    if total <= 1.0e-9 || target_positions.is_empty() {
        return 0.0;
    }
    let target = target_positions
        .iter()
        .map(|position| slots[*position % RHYTHM_SLOTS])
        .sum::<f64>();
    let fraction_of_grid = target_positions.len().min(RHYTHM_SLOTS) as f64 / RHYTHM_SLOTS as f64;
    ((target / total - fraction_of_grid) / (1.0 - fraction_of_grid).max(1.0e-9))
        .clamp(0.0, 1.0)
}

fn estimate_affect(input: &MirInput<'_>) -> (f64, f64) {
    let mode_affect = input
        .musical_key
        .map(|key| {
            if key.to_ascii_lowercase().ends_with("major") {
                input.key_confidence.clamp(0.0, 1.0) * 0.65
            } else if key.to_ascii_lowercase().ends_with("minor") {
                -input.key_confidence.clamp(0.0, 1.0) * 0.55
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let brightness_affect = ((input.spectral_centroid_hz - 1_800.0) / 5_000.0).clamp(-0.5, 0.5);
    let loudness_unit = input
        .integrated_lufs
        .filter(|value| value.is_finite())
        .map(|lufs| ((lufs + 32.0) / 24.0).clamp(0.0, 1.0))
        .unwrap_or_else(|| ((input.energy_dbfs + 38.0) / 30.0).clamp(0.0, 1.0));
    let energy_affect = (loudness_unit - 0.5) * 0.30;
    let valence = (mode_affect + brightness_affect * 0.38 + energy_affect).clamp(-1.0, 1.0);

    let tempo_unit = input
        .bpm
        .filter(|bpm| bpm.is_finite())
        .map(|bpm| ((bpm - 55.0) / 145.0).clamp(0.0, 1.0))
        .unwrap_or(0.35);
    let onset_unit = (input.onset_density / 5.0).clamp(0.0, 1.0);
    let brightness_unit = (input.spectral_centroid_hz / 7_000.0).clamp(0.0, 1.0);
    let compression_unit = (1.0 - (input.dynamic_range_db / 24.0).clamp(0.0, 1.0)).clamp(0.0, 1.0);
    let arousal = (0.34 * loudness_unit
        + 0.26 * tempo_unit
        + 0.24 * onset_unit
        + 0.11 * brightness_unit
        + 0.05 * compression_unit)
        .clamp(0.0, 1.0);
    (valence, arousal)
}

fn mood_label(valence: f64, arousal: f64) -> &'static str {
    if arousal >= 0.62 {
        if valence >= 0.28 {
            "Euphoric"
        } else if valence <= -0.28 {
            "Aggressive"
        } else {
            "High Energy"
        }
    } else if arousal <= 0.38 {
        if valence >= 0.22 {
            "Chill"
        } else if valence <= -0.22 {
            "Melancholic"
        } else {
            "Calm"
        }
    } else if valence <= -0.32 {
        "Tense"
    } else if valence >= 0.35 {
        "Uplifting"
    } else {
        "Balanced"
    }
}

fn estimate_style(input: &MirInput<'_>, rhythm_pattern: &str) -> &'static str {
    match rhythm_pattern {
        "One Drop" | "Steppers" | "Clave" => "Roots / Reggae (heuristic)",
        "Dembow" => "Dembow / Reggaeton (heuristic)",
        "Boom Bap" => "Hip-Hop / Boom Bap (heuristic)",
        _ if input.bpm.is_some_and(|bpm| bpm >= 125.0)
            && input.low_band_ratio >= 0.25
            && input.onset_density >= 1.5 =>
        {
            "Club / Bass (heuristic)"
        }
        _ if input.low_band_ratio >= 0.42 && input.spectral_centroid_hz < 2_500.0 => {
            "Bass-heavy / Dub (heuristic)"
        }
        _ => "Unclassified",
    }
}

fn quantized_feature_vector(
    input: &MirInput<'_>,
    rhythm: &RhythmAnalysis,
    valence: f64,
    arousal: f64,
) -> [i8; FEATURE_VECTOR_SIZE] {
    let mut features = [0.0_f64; FEATURE_VECTOR_SIZE];

    // Pitch-class profile and two-bar sixteenth-note groove pattern form the main similarity axes.
    let chroma_mean = input.chroma.iter().sum::<f64>() / input.chroma.len() as f64;
    for (index, value) in input.chroma.iter().enumerate() {
        features[index] = *value - chroma_mean;
    }
    let rhythm_mean = rhythm.slots.iter().sum::<f64>() / rhythm.slots.len() as f64;
    for (index, value) in rhythm.slots.iter().enumerate() {
        features[12 + index] = *value - rhythm_mean;
    }

    let key_mode = input
        .musical_key
        .map(|key| {
            if key.to_ascii_lowercase().ends_with("major") {
                1.0
            } else if key.to_ascii_lowercase().ends_with("minor") {
                -1.0
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let bpm_unit = input
        .bpm
        .filter(|value| value.is_finite())
        .map(|value| ((value - 120.0) / 120.0).clamp(-1.0, 1.0))
        .unwrap_or(0.0);
    let lufs_unit = input
        .integrated_lufs
        .filter(|value| value.is_finite())
        .map(|value| ((value + 24.0) / 24.0).clamp(-1.0, 1.0))
        .unwrap_or_else(|| ((input.energy_dbfs + 30.0) / 30.0).clamp(-1.0, 1.0));
    let descriptors = [
        valence,
        arousal * 2.0 - 1.0,
        bpm_unit,
        input.bpm_confidence.clamp(0.0, 1.0) * 2.0 - 1.0,
        lufs_unit,
        (input.dynamic_range_db / 24.0).clamp(0.0, 1.0) * 2.0 - 1.0,
        (input.onset_density / 5.0).clamp(0.0, 1.0) * 2.0 - 1.0,
        (input.spectral_centroid_hz / 7_000.0).clamp(0.0, 1.0) * 2.0 - 1.0,
        input.low_band_ratio.clamp(0.0, 1.0) * 2.0 - 1.0,
        input.mid_band_ratio.clamp(0.0, 1.0) * 2.0 - 1.0,
        input.high_band_ratio.clamp(0.0, 1.0) * 2.0 - 1.0,
        input.key_confidence.clamp(0.0, 1.0) * 2.0 - 1.0,
        key_mode,
        rhythm.confidence * 2.0 - 1.0,
        rhythm.syncopation.clamp(0.0, 1.0) * 2.0 - 1.0,
        (input.low_band_ratio - input.high_band_ratio).clamp(-1.0, 1.0),
        (input.mid_band_ratio - input.low_band_ratio).clamp(-1.0, 1.0),
        input
            .bpm
            .filter(|value| value.is_finite())
            .map(|value| (value / 240.0).clamp(0.0, 1.0) * 2.0 - 1.0)
            .unwrap_or(0.0),
        (input.energy_dbfs / 48.0).clamp(-1.0, 0.0) * 2.0 + 1.0,
        input
            .chroma
            .iter()
            .copied()
            .fold(0.0_f64, f64::max)
            / input.chroma.iter().sum::<f64>().max(1.0e-9),
    ];
    features[44..].copy_from_slice(&descriptors);

    let norm = features.iter().map(|value| value * value).sum::<f64>().sqrt();
    if norm > 1.0e-12 && norm.is_finite() {
        features.iter_mut().for_each(|value| *value /= norm);
    }
    let mut result = [0_i8; FEATURE_VECTOR_SIZE];
    for (target, value) in result.iter_mut().zip(features) {
        let quantized = (value * 127.0).round().clamp(-127.0, 127.0);
        *target = quantized as i8;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{analyze, analyze_rhythm, MirInput, FEATURE_VECTOR_SIZE, ONSET_FRAME_RATE};

    fn pulse_track(is_onset: impl Fn(usize) -> bool) -> Vec<f32> {
        let bpm = 120.0;
        let duration = 16.0;
        let mut onsets = vec![0.0_f32; (duration * ONSET_FRAME_RATE).ceil() as usize + 1];
        for subdivision in 0..(duration * bpm / 60.0 * 4.0) as usize {
            if is_onset(subdivision) {
                let time = subdivision as f64 * 60.0 / bpm / 4.0;
                let frame = (time * ONSET_FRAME_RATE).round() as usize;
                if let Some(value) = onsets.get_mut(frame) {
                    *value = 1.0;
                }
            }
        }
        onsets
    }

    #[test]
    fn beat_grid_heuristics_recognize_steppers_and_one_drop() {
        let grid = (0..32).map(|beat| beat as f64 * 0.5).collect::<Vec<_>>();
        let steppers = pulse_track(|subdivision| subdivision % 4 == 0);
        let one_drop = pulse_track(|subdivision| subdivision % 16 == 8);
        let dembow = pulse_track(|subdivision| {
            matches!(subdivision % 16, 0 | 4 | 6 | 8 | 11 | 12 | 14)
        });
        let steppers = analyze_rhythm(&steppers, &grid, Some(120.0), 16.0);
        let one_drop = analyze_rhythm(&one_drop, &grid, Some(120.0), 16.0);
        let dembow = analyze_rhythm(&dembow, &grid, Some(120.0), 16.0);
        assert_eq!(steppers.pattern, "Steppers");
        assert_eq!(one_drop.pattern, "One Drop");
        assert_eq!(dembow.pattern, "Dembow");
    }

    #[test]
    fn mir_returns_bounded_affect_and_a_compact_signed_int8_descriptor() {
        let onsets = pulse_track(|subdivision| subdivision % 16 == 0);
        let grid = (0..32).map(|beat| beat as f64 * 0.5).collect::<Vec<_>>();
        let chroma = [1.0_f64; 12];
        let result = analyze(MirInput {
            bpm: Some(120.0),
            bpm_confidence: 0.7,
            beat_grid_seconds: &grid,
            duration_seconds: 16.0,
            musical_key: Some("C major"),
            key_confidence: 0.8,
            energy_dbfs: -15.0,
            dynamic_range_db: 8.0,
            onset_density: 2.0,
            spectral_centroid_hz: 2_400.0,
            low_band_ratio: 0.45,
            mid_band_ratio: 0.35,
            high_band_ratio: 0.20,
            integrated_lufs: Some(-12.0),
            chroma: &chroma,
            onset_strengths: &onsets,
        });
        assert!((-1.0..=1.0).contains(&result.mood_valence));
        assert!((0.0..=1.0).contains(&result.mood_arousal));
        assert_eq!(result.feature_vector.len(), FEATURE_VECTOR_SIZE);
        assert!(result.feature_vector.iter().any(|value| *value != 0));
        assert!(!result.mood_label.is_empty());
        assert!(!result.style_genre.is_empty());
    }
}
