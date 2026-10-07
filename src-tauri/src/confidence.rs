//! Shared MIR confidence policy.
//!
//! The importer stores two machine-listening confidences per track (`bpm_confidence`,
//! `key_confidence`) which the virtual track list renders as a badge and the grid editor uses to
//! decide whether a correction is worth offering. The policy lives here so the DSP heuristics, the
//! SQLite layer and the test-suite all agree on what "trusted" means.

/// Confidences at or above this value are treated as trusted; anything below is a heuristic guess
/// the UI marks amber/orange. `0.7` is the project-wide threshold.
pub const TRUSTED_CONFIDENCE: f64 = 0.7;

/// Hard ceiling for a result the heuristic could not corroborate. It is deliberately just below
/// [`TRUSTED_CONFIDENCE`] so an unverified guess can never be mistaken for a trusted reading by a
/// `>= TRUSTED_CONFIDENCE` comparison.
pub const UNSURE_CONFIDENCE_CEILING: f64 = 0.69;

/// True when a stored confidence may be presented as trusted without a manual correction.
pub fn is_trusted(confidence: f64) -> bool {
    confidence.is_finite() && confidence >= TRUSTED_CONFIDENCE
}

/// Clamps arbitrary input onto the stored `[0, 1]` scale; non-finite input becomes `0.0`.
pub fn normalise(confidence: f64) -> f64 {
    if confidence.is_finite() {
        confidence.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Reports a purely heuristic confidence.
///
/// A heuristic is only allowed to claim a trusted confidence when it corroborated its own answer
/// (for example by finding the same beat period in both halves of the track). When it cannot, the
/// value is pinned below [`TRUSTED_CONFIDENCE`] — the requirement that "unsure heuristics read as
/// uncertain" is enforced here rather than left to the UI.
pub fn heuristic_confidence(raw: f64, corroborated: bool) -> f64 {
    let clamped = normalise(raw);
    if corroborated && clamped >= TRUSTED_CONFIDENCE {
        clamped
    } else {
        clamped.min(UNSURE_CONFIDENCE_CEILING)
    }
}

/// Confidence to record for a value that came from an authoritative source instead of a heuristic,
/// such as an embedded BPM/key tag or a manual correction. Those are trusted by construction.
pub fn trusted_confidence() -> f64 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_sits_exactly_on_the_trusted_boundary() {
        assert!(is_trusted(TRUSTED_CONFIDENCE));
        assert!(is_trusted(1.0));
        assert!(!is_trusted(0.699_999));
        assert!(!is_trusted(UNSURE_CONFIDENCE_CEILING));
        assert!(!is_trusted(f64::NAN));
    }

    #[test]
    fn uncorroborated_heuristics_can_never_read_as_trusted() {
        assert!(heuristic_confidence(1.0, false) < TRUSTED_CONFIDENCE);
        assert_eq!(heuristic_confidence(1.0, false), UNSURE_CONFIDENCE_CEILING);
        assert_eq!(heuristic_confidence(0.42, false), 0.42);
        assert_eq!(heuristic_confidence(f64::NAN, true), 0.0);
        assert_eq!(heuristic_confidence(-3.0, true), 0.0);
    }

    #[test]
    fn corroborated_heuristics_keep_their_margin_but_stay_bounded() {
        assert_eq!(heuristic_confidence(0.93, true), 0.93);
        assert_eq!(heuristic_confidence(1.4, true), 1.0);
        assert_eq!(heuristic_confidence(0.4, true), 0.4);
        assert_eq!(trusted_confidence(), 1.0);
    }
}
