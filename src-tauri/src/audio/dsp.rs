use super::{AudioParameters, StereoFrame};
use std::f32::consts::PI;

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn low_pass(sample_rate: f32, cutoff_hz: f32, q: f32) -> Self {
        let omega = 2.0 * PI * cutoff_hz / sample_rate;
        let cos = omega.cos();
        let sin = omega.sin();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos) * 0.5) / a0,
            b1: (1.0 - cos) / a0,
            b2: ((1.0 - cos) * 0.5) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn high_pass(sample_rate: f32, cutoff_hz: f32, q: f32) -> Self {
        let omega = 2.0 * PI * cutoff_hz / sample_rate;
        let cos = omega.cos();
        let sin = omega.sin();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 + cos) * 0.5) / a0,
            b1: (-(1.0 + cos)) / a0,
            b2: ((1.0 + cos) * 0.5) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        // Transposed direct-form II; two state values and five multiplies/sample.
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

#[derive(Default)]
struct Allpass {
    delay: Vec<f32>,
    index: usize,
    gain: f32,
}

impl Allpass {
    fn new(delay_samples: usize, gain: f32) -> Self {
        Self {
            delay: vec![0.0; delay_samples.max(1)],
            index: 0,
            gain,
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let delayed = self.delay[self.index];
        let output = delayed - self.gain * input;
        self.delay[self.index] = input + self.gain * output;
        self.index += 1;
        if self.index == self.delay.len() {
            self.index = 0;
        }
        output
    }
}

struct TapeDelay {
    buffer: Vec<StereoFrame>,
    write_index: usize,
    sample_rate: f32,
    phase: f32,
    tone_state: StereoFrame,
}

impl TapeDelay {
    fn new(sample_rate: f32) -> Self {
        Self {
            // 2.1 seconds covers the exposed 2 s maximum plus modulation headroom.
            buffer: vec![StereoFrame::default(); (sample_rate * 2.1).ceil() as usize],
            write_index: 0,
            sample_rate,
            phase: 0.0,
            tone_state: StereoFrame::default(),
        }
    }

    #[inline]
    fn process(&mut self, dry: StereoFrame, parameters: &AudioParameters) -> StereoFrame {
        let length = self.buffer.len();
        let base_delay = parameters.tape_delay_ms * self.sample_rate * 0.001;
        let wow = self.phase.sin() * (self.sample_rate * 0.0015);
        let flutter = (self.phase * 17.0).sin() * (self.sample_rate * 0.00008);
        let delay = (base_delay + wow + flutter).clamp(1.0, (length - 2) as f32);
        let read_position = (self.write_index as f32 - delay).rem_euclid(length as f32);
        let read_index = read_position.floor() as usize;
        let next_index = (read_index + 1) % length;
        let fraction = read_position - read_index as f32;
        let first = self.buffer[read_index];
        let second = self.buffer[next_index];
        let delayed = StereoFrame {
            left: first.left + (second.left - first.left) * fraction,
            right: first.right + (second.right - first.right) * fraction,
        };

        // The tape head rolls off high frequencies on every repeat.
        let tone_alpha = 0.28;
        self.tone_state.left += tone_alpha * (delayed.left - self.tone_state.left);
        self.tone_state.right += tone_alpha * (delayed.right - self.tone_state.right);
        let feedback = parameters.tape_feedback;
        self.buffer[self.write_index] = StereoFrame {
            left: tape_saturate(dry.left + self.tone_state.left * feedback),
            right: tape_saturate(dry.right + self.tone_state.right * feedback),
        };
        self.write_index += 1;
        if self.write_index == length {
            self.write_index = 0;
        }
        self.phase += 2.0 * PI * 0.31 / self.sample_rate;
        if self.phase >= 2.0 * PI {
            self.phase -= 2.0 * PI;
        }

        let wet = parameters.tape_mix;
        StereoFrame {
            left: dry.left * (1.0 - wet) + delayed.left * wet,
            right: dry.right * (1.0 - wet) + delayed.right * wet,
        }
    }
}

#[inline]
fn tape_saturate(value: f32) -> f32 {
    // Rational tanh approximation, bounded to keep the feedback loop stable.
    let value = value.clamp(-3.0, 3.0);
    let square = value * value;
    value * (27.0 + square) / (27.0 + 9.0 * square)
}

struct SpringReverb {
    diffuser_left_a: Allpass,
    diffuser_left_b: Allpass,
    diffuser_right_a: Allpass,
    diffuser_right_b: Allpass,
    tank_left: Vec<f32>,
    tank_right: Vec<f32>,
    index_left: usize,
    index_right: usize,
    damping_left: f32,
    damping_right: f32,
    damping_coefficient: f32,
}

impl SpringReverb {
    fn new(sample_rate: f32) -> Self {
        let samples = |seconds: f32| (sample_rate * seconds).round().max(1.0) as usize;
        Self {
            diffuser_left_a: Allpass::new(samples(0.0017), 0.68),
            diffuser_left_b: Allpass::new(samples(0.0023), 0.62),
            diffuser_right_a: Allpass::new(samples(0.0019), 0.68),
            diffuser_right_b: Allpass::new(samples(0.0027), 0.62),
            tank_left: vec![0.0; samples(0.0311)],
            tank_right: vec![0.0; samples(0.0377)],
            index_left: 0,
            index_right: 0,
            damping_left: 0.0,
            damping_right: 0.0,
            damping_coefficient: 0.16,
        }
    }

    #[inline]
    fn process(&mut self, dry: StereoFrame, parameters: &AudioParameters) -> StereoFrame {
        let input_left = self
            .diffuser_left_b
            .process(self.diffuser_left_a.process(dry.left));
        let input_right = self
            .diffuser_right_b
            .process(self.diffuser_right_a.process(dry.right));
        let delayed_left = self.tank_left[self.index_left];
        let delayed_right = self.tank_right[self.index_right];

        self.damping_left += self.damping_coefficient * (delayed_left - self.damping_left);
        self.damping_right += self.damping_coefficient * (delayed_right - self.damping_right);
        let decay = parameters.spring_decay;
        self.tank_left[self.index_left] = input_left + self.damping_right * decay;
        self.tank_right[self.index_right] = input_right + self.damping_left * decay;

        self.index_left += 1;
        if self.index_left == self.tank_left.len() {
            self.index_left = 0;
        }
        self.index_right += 1;
        if self.index_right == self.tank_right.len() {
            self.index_right = 0;
        }

        // Cross-coupled, damped delay tank plus the short dispersive allpass chain
        // approximates a compact spring tank without allocating in the callback.
        let wet = parameters.spring_mix;
        StereoFrame {
            left: dry.left * (1.0 - wet) + (delayed_left + input_right * 0.18) * wet,
            right: dry.right * (1.0 - wet) + (delayed_right + input_left * 0.18) * wet,
        }
    }
}

pub struct DspChain {
    sample_rate: f32,
    high_pass: [Biquad; 2],
    low_pass: [Biquad; 2],
    isolator_low: [Biquad; 2],
    isolator_high: [Biquad; 2],
    high_pass_cutoff: f32,
    low_pass_cutoff: f32,
    tape_delay: TapeDelay,
    spring_reverb: SpringReverb,
}

impl DspChain {
    pub fn new(sample_rate: u32) -> Self {
        let rate = sample_rate as f32;
        let high_pass_cutoff = 30.0_f32.min(rate * 0.2);
        let low_pass_cutoff = 18_000.0_f32.min(rate * 0.45);
        Self {
            sample_rate: rate,
            high_pass: [Biquad::high_pass(rate, high_pass_cutoff, 0.707); 2],
            low_pass: [Biquad::low_pass(rate, low_pass_cutoff, 0.707); 2],
            isolator_low: [Biquad::low_pass(rate, 250.0_f32.min(rate * 0.2), 0.707); 2],
            isolator_high: [Biquad::high_pass(rate, 2_500.0_f32.min(rate * 0.4), 0.707); 2],
            high_pass_cutoff,
            low_pass_cutoff,
            tape_delay: TapeDelay::new(rate),
            spring_reverb: SpringReverb::new(rate),
        }
    }

    pub fn prepare(&mut self, parameters: &AudioParameters) {
        self.update_filter_coefficients(parameters);
    }

    #[inline]
    pub fn process(&mut self, input: StereoFrame, parameters: &AudioParameters) -> StereoFrame {
        let mut left = input.left;
        let mut right = input.right;
        if parameters.high_pass_enabled {
            left = self.high_pass[0].process(left);
            right = self.high_pass[1].process(right);
        }
        if parameters.low_pass_enabled {
            left = self.low_pass[0].process(left);
            right = self.low_pass[1].process(right);
        }

        let low_left = self.isolator_low[0].process(left);
        let low_right = self.isolator_low[1].process(right);
        let high_left = self.isolator_high[0].process(left);
        let high_right = self.isolator_high[1].process(right);
        let mid_left = left - low_left - high_left;
        let mid_right = right - low_right - high_right;
        let bass_gain = if parameters.kill_bass {
            0.0
        } else {
            parameters.bass_gain
        };
        let mid_gain = if parameters.kill_mid {
            0.0
        } else {
            parameters.mid_gain
        };
        let high_gain = if parameters.kill_high {
            0.0
        } else {
            parameters.high_gain
        };
        let isolated = StereoFrame {
            left: low_left * bass_gain + mid_left * mid_gain + high_left * high_gain,
            right: low_right * bass_gain + mid_right * mid_gain + high_right * high_gain,
        };
        let delayed = self.tape_delay.process(isolated, parameters);
        let reverberated = self.spring_reverb.process(delayed, parameters);
        StereoFrame {
            left: tape_saturate(reverberated.left * parameters.master_gain),
            right: tape_saturate(reverberated.right * parameters.master_gain),
        }
    }

    fn update_filter_coefficients(&mut self, parameters: &AudioParameters) {
        let maximum_cutoff = self.sample_rate * 0.45;
        let high_pass_cutoff = parameters.high_pass_hz.clamp(10.0, maximum_cutoff);
        if high_pass_cutoff.to_bits() != self.high_pass_cutoff.to_bits() {
            self.high_pass_cutoff = high_pass_cutoff;
            self.high_pass = [
                Biquad::high_pass(self.sample_rate, high_pass_cutoff, 0.707),
                Biquad::high_pass(self.sample_rate, high_pass_cutoff, 0.707),
            ];
        }

        let low_pass_cutoff = parameters.low_pass_hz.clamp(20.0, maximum_cutoff);
        if low_pass_cutoff.to_bits() != self.low_pass_cutoff.to_bits() {
            self.low_pass_cutoff = low_pass_cutoff;
            self.low_pass = [
                Biquad::low_pass(self.sample_rate, low_pass_cutoff, 0.707),
                Biquad::low_pass(self.sample_rate, low_pass_cutoff, 0.707),
            ];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tape_saturate, Biquad};

    #[test]
    fn saturator_is_finite_and_bounded() {
        for value in [-100.0, -3.0, -1.0, 0.0, 1.0, 3.0, 100.0] {
            let output = tape_saturate(value);
            assert!(output.is_finite());
            assert!(output.abs() <= 1.0);
        }
    }

    #[test]
    fn biquad_impulse_response_remains_finite() {
        let mut low_pass = Biquad::low_pass(48_000.0, 1_000.0, 0.707);
        let mut high_pass = Biquad::high_pass(48_000.0, 1_000.0, 0.707);
        for index in 0..20_000 {
            let impulse = if index == 0 { 1.0 } else { 0.0 };
            assert!(low_pass.process(impulse).is_finite());
            assert!(high_pass.process(impulse).is_finite());
        }
    }
}
