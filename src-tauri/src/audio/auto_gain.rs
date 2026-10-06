//! Per-track R128 gain compensation with a short click-free ramp.
//!
//! The requested LUFS target is a loudness offset, not a promise that every track will
//! measure at that exact value after the master limiter, EQ, crossfade, and other DSP.

const DEFAULT_RAMP_MS: f32 = 8.0;
const MAX_GAIN_UP_DB: f32 = 24.0;
const MAX_GAIN_DOWN_DB: f32 = -48.0;

#[derive(Debug, Clone, Copy)]
pub(super) struct AutoGainRamp {
    current_gain: f32,
    start_gain: f32,
    target_gain: f32,
    ramp_length: usize,
    ramp_position: usize,
    last_target_db: f32,
}

impl AutoGainRamp {
    pub(super) fn new(sample_rate: u32) -> Self {
        Self {
            current_gain: 1.0,
            start_gain: 1.0,
            target_gain: 1.0,
            ramp_length: ramp_frames(sample_rate, DEFAULT_RAMP_MS),
            ramp_position: ramp_frames(sample_rate, DEFAULT_RAMP_MS),
            last_target_db: 0.0,
        }
    }

    /// Set an R128 gain offset. The ramp is linear in amplitude with a smoothstep
    /// interpolation, and is advanced once for each output sample frame.
    #[inline]
    pub(super) fn next_gain(&mut self, requested_db: f32) -> f32 {
        let requested_db = if requested_db.is_finite() {
            requested_db.clamp(MAX_GAIN_DOWN_DB, MAX_GAIN_UP_DB)
        } else {
            0.0
        };
        if (requested_db - self.last_target_db).abs() > 1.0e-4 {
            self.start_gain = self.current_gain;
            self.target_gain = 10.0_f32.powf(requested_db / 20.0);
            self.ramp_position = 0;
            self.last_target_db = requested_db;
        }

        if self.ramp_position < self.ramp_length {
            self.ramp_position += 1;
            let linear = self.ramp_position as f32 / self.ramp_length as f32;
            let eased = linear * linear * (3.0 - 2.0 * linear);
            self.current_gain = self.start_gain + (self.target_gain - self.start_gain) * eased;
        } else {
            self.current_gain = self.target_gain;
        }
        self.current_gain
    }

    pub(super) fn target_db(enabled: bool, integrated_lufs: Option<f32>, target_lufs: f32) -> f32 {
        if !enabled {
            return 0.0;
        }
        let Some(measured) =
            integrated_lufs.filter(|value| value.is_finite() && (-70.0..=10.0).contains(value))
        else {
            return 0.0;
        };
        let target = if target_lufs.is_finite() {
            target_lufs.clamp(-36.0, -6.0)
        } else {
            -14.0
        };
        (target - measured).clamp(MAX_GAIN_DOWN_DB, MAX_GAIN_UP_DB)
    }
}

fn ramp_frames(sample_rate: u32, milliseconds: f32) -> usize {
    ((sample_rate.max(1) as f32 * milliseconds / 1_000.0).round() as usize).max(1)
}

#[cfg(test)]
mod tests {
    use super::AutoGainRamp;

    #[test]
    fn r128_gain_offset_uses_adjustable_target_and_is_safely_bounded() {
        assert!((AutoGainRamp::target_db(true, Some(-20.0), -14.0) - 6.0).abs() < 1e-6);
        assert!((AutoGainRamp::target_db(true, Some(-20.0), -12.0) - 8.0).abs() < 1e-6);
        assert_eq!(AutoGainRamp::target_db(false, Some(-20.0), -14.0), 0.0);
        assert_eq!(AutoGainRamp::target_db(true, None, -14.0), 0.0);
        assert_eq!(AutoGainRamp::target_db(true, Some(-80.0), -14.0), 0.0);
    }

    #[test]
    fn target_changes_have_a_finite_eight_millisecond_smooth_ramp() {
        let mut ramp = AutoGainRamp::new(48_000);
        let first = ramp.next_gain(12.0);
        assert!(first > 1.0 && first < 10.0_f32.powf(12.0 / 20.0));
        let mut previous = first;
        for _ in 1..384 {
            let current = ramp.next_gain(12.0);
            assert!(current >= previous);
            previous = current;
        }
        assert!((previous - 10.0_f32.powf(12.0 / 20.0)).abs() < 1.0e-5);

        let before_disable = previous;
        let after_disable = ramp.next_gain(0.0);
        assert!(after_disable < before_disable);
        assert!(after_disable > 1.0);
    }

    #[test]
    fn a_missing_measurement_does_not_apply_gain() {
        let mut ramp = AutoGainRamp::new(44_100);
        for _ in 0..352 {
            let gain = ramp.next_gain(AutoGainRamp::target_db(true, None, -14.0));
            assert!((gain - 1.0).abs() < 1.0e-6);
        }
    }
}
