//! MIR-aware, deterministic local DJ transition planning.
//!
//! This module consumes already-local analysis metadata only. It does not download audio or call
//! a remote inference service; scores are heuristics, not a claim of a learned DJ model.

#[derive(Debug, Clone, Copy)]
pub(crate) enum TransitionStyle {
    DeepRoots,
    ClubHype,
    AggressiveClash,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MirProfile<'a> {
    pub bpm: Option<f64>,
    pub musical_key: Option<&'a str>,
    pub integrated_lufs: Option<f64>,
    pub mood_valence: Option<f64>,
    pub mood_arousal: Option<f64>,
    pub mood_label: Option<&'a str>,
    pub rhythm_pattern: Option<&'a str>,
    pub rhythm_confidence: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MirTransitionPlan {
    pub compatibility: f64,
    pub mood_compatibility: Option<f64>,
    pub rhythm_compatibility: Option<f64>,
    pub key_compatibility: Option<f64>,
    pub loudness_delta_db: Option<f64>,
    pub crossfade_bars: u32,
    pub ideal_drop_cue_ms: u64,
    pub bass_swap: bool,
    pub eq_sweep: bool,
    /// An exponent below one brings an energetic incoming track up earlier.
    pub incoming_curve_exponent: f32,
    pub bass_handoff_start: f32,
    pub bass_handoff_end: f32,
}

pub(crate) fn plan_transition(
    outgoing: MirProfile<'_>,
    incoming: MirProfile<'_>,
    style: TransitionStyle,
    same_riddim_family: bool,
    incoming_beats_seconds: &[f64],
    fallback_drop_ms: Option<u64>,
) -> MirTransitionPlan {
    let mood = mood_compatibility(outgoing, incoming);
    let rhythm = rhythm_compatibility(outgoing, incoming);
    let key = key_compatibility(outgoing.musical_key, incoming.musical_key);
    let loudness_delta = outgoing
        .integrated_lufs
        .zip(incoming.integrated_lufs)
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .map(|(a, b)| b - a);
    let loudness = loudness_delta.map(|delta| (1.0 - delta.abs() / 18.0).clamp(0.0, 1.0));
    let tempo = tempo_compatibility(outgoing.bpm, incoming.bpm);

    let mut weighted_sum = 0.0;
    let mut weight_total = 0.0;
    for (score, weight) in [
        (mood, 0.32),
        (rhythm, 0.28),
        (key, 0.20),
        (loudness, 0.12),
        (tempo, 0.08),
    ] {
        if let Some(score) = score {
            weighted_sum += score * weight;
            weight_total += weight;
        }
    }
    let compatibility = if weight_total > 0.0 {
        (weighted_sum / weight_total).clamp(0.0, 1.0)
    } else {
        0.5
    };

    let arousal_delta = outgoing
        .mood_arousal
        .zip(incoming.mood_arousal)
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .map(|(a, b)| b - a);
    let crossfade_bars = match style {
        TransitionStyle::DeepRoots => {
            if key.is_some_and(|score| score < 0.4) || rhythm.is_some_and(|score| score < 0.45) {
                4
            } else if arousal_delta.is_some_and(|delta| delta.abs() >= 0.35) {
                6
            } else {
                8
            }
        }
        TransitionStyle::ClubHype => 1,
        TransitionStyle::AggressiveClash => 2,
    };
    let bass_swap = matches!(style, TransitionStyle::DeepRoots)
        && (same_riddim_family || rhythm.is_some_and(|score| score >= 0.65));
    let eq_sweep = compatibility < 0.72
        || rhythm.is_some_and(|score| score < 0.5)
        || key.is_some_and(|score| score < 0.4);
    let incoming_arousal = incoming.mood_arousal.filter(|value| value.is_finite());
    let incoming_curve_exponent = match incoming_arousal {
        Some(value) if value >= 0.68 => 0.82,
        Some(value) if value <= 0.34 => 1.18,
        _ => 1.0,
    };
    let bass_handoff_start = if compatibility >= 0.72 { 0.34 } else { 0.45 };
    let bass_handoff_end = if compatibility >= 0.72 { 0.72 } else { 0.82 };

    MirTransitionPlan {
        compatibility,
        mood_compatibility: mood,
        rhythm_compatibility: rhythm,
        key_compatibility: key,
        loudness_delta_db: loudness_delta,
        crossfade_bars,
        ideal_drop_cue_ms: choose_drop_cue(incoming, incoming_beats_seconds, fallback_drop_ms),
        bass_swap,
        eq_sweep,
        incoming_curve_exponent,
        bass_handoff_start,
        bass_handoff_end,
    }
}

/// Ranking contribution for choosing the next local track. Values are 0..1 and missing MIR fields
/// remain neutral rather than disqualifying older library entries without analysis.
pub(crate) fn transition_score(outgoing: MirProfile<'_>, incoming: MirProfile<'_>) -> f64 {
    let plan = plan_transition(
        outgoing,
        incoming,
        TransitionStyle::DeepRoots,
        false,
        &[],
        None,
    );
    plan.compatibility
}

fn mood_compatibility(outgoing: MirProfile<'_>, incoming: MirProfile<'_>) -> Option<f64> {
    let valence = outgoing
        .mood_valence
        .zip(incoming.mood_valence)
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .map(|(a, b)| (1.0 - (a - b).abs() * 0.5).clamp(0.0, 1.0));
    let arousal = outgoing
        .mood_arousal
        .zip(incoming.mood_arousal)
        .filter(|(a, b)| a.is_finite() && b.is_finite())
        .map(|(a, b)| (1.0 - (a - b).abs()).clamp(0.0, 1.0));
    match (valence, arousal) {
        (Some(valence), Some(arousal)) => Some(0.45 * valence + 0.55 * arousal),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => {
            let outgoing = outgoing.mood_label?;
            let incoming = incoming.mood_label?;
            let outgoing = normalize_label(outgoing);
            let incoming = normalize_label(incoming);
            Some(if outgoing == incoming { 0.9 } else { 0.35 })
        }
    }
}

fn rhythm_compatibility(outgoing: MirProfile<'_>, incoming: MirProfile<'_>) -> Option<f64> {
    let outgoing_pattern = outgoing.rhythm_pattern?;
    let incoming_pattern = incoming.rhythm_pattern?;
    let outgoing_confidence = outgoing.rhythm_confidence.unwrap_or(1.0).clamp(0.0, 1.0);
    let incoming_confidence = incoming.rhythm_confidence.unwrap_or(1.0).clamp(0.0, 1.0);
    let confidence = outgoing_confidence.min(incoming_confidence);
    let left = rhythm_family(outgoing_pattern);
    let right = rhythm_family(incoming_pattern);
    let raw = match (left, right) {
        (Some(left), Some(right)) if left == right => 1.0,
        (Some(RhythmFamily::Roots), Some(RhythmFamily::Steppers))
        | (Some(RhythmFamily::Steppers), Some(RhythmFamily::Roots))
        | (Some(RhythmFamily::Dancehall), Some(RhythmFamily::Roots))
        | (Some(RhythmFamily::Roots), Some(RhythmFamily::Dancehall)) => 0.72,
        (Some(_), Some(_)) => 0.25,
        _ if normalize_label(outgoing_pattern) == normalize_label(incoming_pattern) => 0.8,
        _ => return None,
    };
    Some(0.5 + (raw - 0.5) * confidence)
}

#[derive(PartialEq, Eq)]
enum RhythmFamily {
    Roots,
    Steppers,
    Dancehall,
    Club,
    HipHop,
}

fn rhythm_family(value: &str) -> Option<RhythmFamily> {
    let value = normalize_label(value);
    if value.contains("one drop") || value.contains("roots") || value.contains("rub a dub") {
        Some(RhythmFamily::Roots)
    } else if value.contains("stepper") || value.contains("four on the floor") {
        Some(if value.contains("four on the floor") {
            RhythmFamily::Club
        } else {
            RhythmFamily::Steppers
        })
    } else if value.contains("dancehall") || value.contains("dembow") || value.contains("reggae") {
        Some(RhythmFamily::Dancehall)
    } else if value.contains("club") || value.contains("house") || value.contains("disco") {
        Some(RhythmFamily::Club)
    } else if value.contains("hip hop") || value.contains("hiphop") || value.contains("trap") {
        Some(RhythmFamily::HipHop)
    } else {
        None
    }
}

fn key_compatibility(outgoing: Option<&str>, incoming: Option<&str>) -> Option<f64> {
    let (left_key, left_mode) = parse_key(outgoing?)?;
    let (right_key, right_mode) = parse_key(incoming?)?;
    let distance = (left_key as i16 - right_key as i16).rem_euclid(12);
    let circle_score = match distance {
        0 => 1.0,
        5 | 7 => 0.88,
        2 | 10 => 0.72,
        3 | 9 => 0.64,
        1 | 11 => 0.38,
        _ => 0.25,
    };
    let pitch_distance = (left_key as i16 - right_key as i16).rem_euclid(12);
    let relative_minor = matches!(pitch_distance, 3 | 9) && left_mode != right_mode;
    Some(if relative_minor {
        circle_score.max(0.9)
    } else if distance == 0 && left_mode != right_mode {
        0.8
    } else {
        circle_score
    })
}

fn parse_key(value: &str) -> Option<(u8, bool)> {
    let mut parts = value.split_whitespace();
    let note = parts
        .next()?
        .trim()
        .to_ascii_lowercase()
        .replace('♯', "#")
        .replace('♭', "b");
    let pitch = match note.as_str() {
        "c" | "b#" => 0,
        "c#" | "db" => 1,
        "d" => 2,
        "d#" | "eb" => 3,
        "e" | "fb" => 4,
        "f" | "e#" => 5,
        "f#" | "gb" => 6,
        "g" => 7,
        "g#" | "ab" => 8,
        "a" => 9,
        "a#" | "bb" => 10,
        "b" | "cb" => 11,
        _ => return None,
    };
    let mode = parts
        .next()
        .is_some_and(|part| matches!(part.to_ascii_lowercase().as_str(), "minor" | "min" | "m"));
    Some((pitch, mode))
}

fn tempo_compatibility(outgoing: Option<f64>, incoming: Option<f64>) -> Option<f64> {
    let (outgoing, incoming) = outgoing.zip(incoming)?;
    if !outgoing.is_finite() || !incoming.is_finite() || outgoing <= 0.0 || incoming <= 0.0 {
        return None;
    }
    let error = [0.5, 1.0, 2.0]
        .into_iter()
        .map(|factor| {
            let candidate = incoming * factor;
            let rate = (outgoing / candidate).clamp(0.8, 1.2);
            (candidate * rate - outgoing).abs() / outgoing
        })
        .fold(f64::INFINITY, f64::min);
    Some((1.0 - error * 3.0).clamp(0.0, 1.0))
}

fn choose_drop_cue(
    profile: MirProfile<'_>,
    beat_times_seconds: &[f64],
    fallback_drop_ms: Option<u64>,
) -> u64 {
    let fallback = fallback_drop_ms.unwrap_or(0).min(86_400_000);
    let pattern = profile
        .rhythm_pattern
        .unwrap_or_default()
        .to_ascii_lowercase();
    let stride = if pattern.contains("dembow") || pattern.contains("dancehall") {
        2
    } else {
        4
    };
    for (index, beat) in beat_times_seconds.iter().enumerate() {
        if !beat.is_finite() || *beat < 0.0 || *beat > 86_400.0 {
            continue;
        }
        let millis = (*beat * 1_000.0).round() as u64;
        if millis >= fallback && index % stride == 0 {
            return millis;
        }
    }
    if !beat_times_seconds.is_empty() {
        if let Some(beat) = beat_times_seconds
            .iter()
            .copied()
            .find(|beat| beat.is_finite() && *beat >= 0.0 && *beat <= 86_400.0)
        {
            return (beat * 1_000.0).round() as u64;
        }
    }
    if let Some(bpm) = profile.bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0) {
        let bar_ms = 4.0 * 60_000.0 / bpm;
        return ((fallback as f64 / bar_ms).ceil() * bar_ms)
            .round()
            .min(86_400_000.0) as u64;
    }
    fallback
}

fn normalize_label(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .replace('_', " ")
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{plan_transition, MirProfile, TransitionStyle};

    fn profile(
        mood_valence: f64,
        mood_arousal: f64,
        rhythm: &'static str,
        key: &'static str,
    ) -> MirProfile<'static> {
        MirProfile {
            bpm: Some(120.0),
            musical_key: Some(key),
            integrated_lufs: Some(-14.0),
            mood_valence: Some(mood_valence),
            mood_arousal: Some(mood_arousal),
            mood_label: None,
            rhythm_pattern: Some(rhythm),
            rhythm_confidence: Some(0.9),
        }
    }

    #[test]
    fn similar_mood_rhythm_key_and_loudness_produce_a_layerable_plan() {
        let plan = plan_transition(
            profile(0.6, 0.65, "one_drop", "A minor"),
            profile(0.5, 0.68, "roots", "C major"),
            TransitionStyle::DeepRoots,
            true,
            &[0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5],
            Some(1_000),
        );
        assert!(plan.compatibility > 0.7);
        assert_eq!(plan.crossfade_bars, 8);
        assert!(plan.bass_swap);
        assert_eq!(plan.ideal_drop_cue_ms, 2_000);
    }

    #[test]
    fn an_incompatible_key_or_pattern_uses_shorter_crossfade_and_eq_sweep() {
        let plan = plan_transition(
            profile(-0.8, 0.9, "dembow", "C major"),
            profile(0.8, 0.1, "steppers", "F# minor"),
            TransitionStyle::DeepRoots,
            false,
            &[0.0, 0.5, 1.0, 1.5, 2.0],
            None,
        );
        assert_eq!(plan.crossfade_bars, 4);
        assert!(plan.eq_sweep);
        assert!(!plan.bass_swap);
    }

    #[test]
    fn key_mode_parser_handles_major_minor_and_enharmonic_spellings() {
        let same_mode = plan_transition(
            MirProfile {
                musical_key: Some("C# major"),
                ..MirProfile::default()
            },
            MirProfile {
                musical_key: Some("Db major"),
                ..MirProfile::default()
            },
            TransitionStyle::DeepRoots,
            false,
            &[],
            None,
        );
        assert_eq!(same_mode.key_compatibility, Some(1.0));
        let parallel_mode = plan_transition(
            MirProfile {
                musical_key: Some("C major"),
                ..MirProfile::default()
            },
            MirProfile {
                musical_key: Some("C minor"),
                ..MirProfile::default()
            },
            TransitionStyle::DeepRoots,
            false,
            &[],
            None,
        );
        assert_eq!(parallel_mode.key_compatibility, Some(0.8));
    }

    #[test]
    fn absent_mir_fields_yield_neutral_compatibility_without_rejecting_track() {
        let plan = plan_transition(
            MirProfile::default(),
            MirProfile::default(),
            TransitionStyle::DeepRoots,
            false,
            &[],
            None,
        );
        assert_eq!(plan.compatibility, 0.5);
        assert_eq!(plan.ideal_drop_cue_ms, 0);
    }
}
