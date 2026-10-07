//! Spleeter 2-stem inference for the cleared default separator.
//!
//! The graphs come from the pinned `sherpa-onnx-spleeter-2stems` archive (MIT Spleeter weights by
//! Deezer, Apache-2.0 ONNX conversion by sherpa-onnx) and are installed by
//! [`crate::audio::model_loader`]. Layout and preprocessing mirror the reference implementation in
//! sherpa-onnx's `offline-source-separation-spleeter-impl.h`:
//!
//! * 44.1 kHz stereo, periodic Hann STFT with `n_fft = 4096`, `hop = 1024`, `center = false`
//! * only the first 1024 of 2049 frequency bins are fed to the U-Nets, matching Spleeter's
//!   configuration and the 1024-bin model contract
//! * the model sees the magnitude `sqrt(re^2 + im^2)` as float32 `[2, splits, 512, 1024]`
//! * both stems come back as spectrograms and are turned into Spleeter's ratio mask
//!   `(stem^2 + eps/2) / (vocals^2 + accompaniment^2 + eps)` before the inverse STFT
//!
//! Processing is streamed in 512-frame chunks with a weighted overlap-add inside a sliding window,
//! so a full track never has to be resident in memory. The tail that lies outside the last complete
//! frame is padded with silence to keep the stem timeline identical to the source (Spleeter's
//! `center = false` framing cannot cover the final `n_fft - hop` samples by construction).

use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader};
use ort::{
    session::Session,
    value::{Tensor, TensorElementType, ValueType},
};
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::audio::model_loader::{
    read_spleeter_descriptor, sha256_file, SPLEETER_BINS, SPLEETER_FRAMES, SPLEETER_HOP_LENGTH,
    SPLEETER_N_FFT, SPLEETER_SAMPLE_RATE,
};

/// Spleeter's stabiliser for the ratio mask; the half-epsilon keeps silent bins from flipping.
const SPLEETER_EPSILON: f32 = 1.0e-10;
/// ChampionVibe stem names in model-stem order: Spleeter stem 0 is vocals, stem 1 accompaniment.
pub(crate) const SPLEETER_STEM_NAMES: [&str; 2] = ["acapella", "instrumental"];

pub(crate) struct SpleeterSeparator {
    vocals: Session,
    accompaniment: Session,
    input_name: String,
    output_name: String,
    sample_rate: u32,
    backend: &'static str,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
}

impl SpleeterSeparator {
    pub(crate) fn load(descriptor_path: &Path, threads: usize, use_directml: bool) -> Result<Self> {
        let descriptor = read_spleeter_descriptor(descriptor_path)?;
        anyhow::ensure!(
            descriptor.kind == "spleeter-2stems",
            "the installed manifest is not a Spleeter 2-stem profile"
        );
        anyhow::ensure!(
            descriptor.sample_rate == SPLEETER_SAMPLE_RATE,
            "Spleeter artifacts must run at 44.1 kHz, found {} Hz",
            descriptor.sample_rate
        );
        anyhow::ensure!(
            descriptor.n_fft == SPLEETER_N_FFT
                && descriptor.hop_length == SPLEETER_HOP_LENGTH
                && descriptor.frames == SPLEETER_FRAMES
                && descriptor.bins == SPLEETER_BINS,
            "the installed Spleeter manifest does not match the verified 4096/1024/512/1024 contract"
        );

        let vocals_path = descriptor
            .models
            .get("vocals")
            .context("the Spleeter manifest is missing its vocals graph")?;
        let accompaniment_path = descriptor
            .models
            .get("accompaniment")
            .context("the Spleeter manifest is missing its accompaniment graph")?;
        for (stem, path_text) in [
            ("vocals", vocals_path),
            ("accompaniment", accompaniment_path),
        ] {
            let path = Path::new(path_text);
            anyhow::ensure!(
                path.is_file(),
                "Spleeter {stem} graph is missing at {}",
                path.display()
            );
            if let Some(expected) = descriptor.sha256.get(stem) {
                let actual = sha256_file(path)?;
                anyhow::ensure!(
                    actual.eq_ignore_ascii_case(expected),
                    "Spleeter {stem} graph failed its recorded SHA-256 check; reinstall the model"
                );
            }
        }

        let threads = threads.clamp(1, 8);
        let (mut vocals, backend) =
            super::separator::create_session(Path::new(vocals_path), threads, use_directml)?;
        let (mut accompaniment, accompaniment_backend) =
            super::separator::create_session(Path::new(accompaniment_path), threads, use_directml)?;
        anyhow::ensure!(
            backend == accompaniment_backend,
            "both Spleeter graphs must run on the same execution provider"
        );
        validate_spleeter_session(&vocals)?;
        validate_spleeter_session(&accompaniment)?;
        let input_name = vocals
            .inputs()
            .first()
            .context("Spleeter vocals graph has no input")?
            .name()
            .to_owned();
        let output_name = vocals
            .outputs()
            .first()
            .context("Spleeter vocals graph has no output")?
            .name()
            .to_owned();

        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(SPLEETER_N_FFT);
        let inverse = planner.plan_fft_inverse(SPLEETER_N_FFT);
        // Periodic Hann, as in torch.stft(center=False, window=hann_window(periodic=True)).
        let window = (0..SPLEETER_N_FFT)
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * index as f32 / SPLEETER_N_FFT as f32;
                0.5 * (1.0 - phase.cos())
            })
            .collect::<Vec<f32>>();

        Ok(Self {
            vocals,
            accompaniment,
            input_name,
            output_name,
            sample_rate: descriptor.sample_rate,
            backend,
            forward,
            inverse,
            window,
        })
    }

    pub(crate) fn backend(&self) -> &'static str {
        self.backend
    }

    pub(crate) fn stem_count(&self) -> usize {
        SPLEETER_STEM_NAMES.len()
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn separate(
        &mut self,
        input_path: &Path,
        output_dir: &Path,
        total_frames: u64,
        cancel: &AtomicBool,
        mut progress: impl FnMut(f64),
    ) -> Result<Vec<PathBuf>> {
        anyhow::ensure!(total_frames > 0, "audio source contains no frames");
        let source_frames =
            usize::try_from(total_frames).context("audio source is too long for this platform")?;
        fs::create_dir_all(output_dir).with_context(|| {
            format!("cannot create stem output folder {}", output_dir.display())
        })?;

        let mut reader = WavReader::open(input_path)
            .with_context(|| format!("cannot open normalized audio {}", input_path.display()))?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.channels == 2
                && spec.sample_rate == SPLEETER_SAMPLE_RATE
                && spec.sample_format == SampleFormat::Int
                && spec.bits_per_sample == 16,
            "Spleeter needs the normalized 44.1 kHz stereo 16-bit workspace WAV"
        );

        let raw_paths: Vec<PathBuf> = SPLEETER_STEM_NAMES
            .iter()
            .map(|name| output_dir.join(format!(".{name}.pcm")))
            .collect();
        let mut raw_files: Vec<BufWriter<File>> = raw_paths
            .iter()
            .map(|path| {
                File::create(path)
                    .with_context(|| format!("cannot create temporary PCM stem {}", path.display()))
                    .map(BufWriter::new)
            })
            .collect::<Result<_>>()?;

        let model_frames = if source_frames >= SPLEETER_N_FFT {
            1 + (source_frames - SPLEETER_N_FFT) / SPLEETER_HOP_LENGTH
        } else {
            0
        };

        let plane = SPLEETER_FRAMES * SPLEETER_BINS;
        let mut tensor = vec![0.0_f32; 2 * plane];
        let mut real = vec![0.0_f32; 2 * plane];
        let mut imag = vec![0.0_f32; 2 * plane];
        let mut spectrum = vec![Complex::new(0.0_f32, 0.0_f32); SPLEETER_N_FFT];
        let mut accumulators = vec![vec![[0.0_f32; 2]; SPLEETER_N_FFT]; SPLEETER_STEM_NAMES.len()];
        let mut window_energy = vec![0.0_f32; SPLEETER_N_FFT];

        // Sliding input window: samples are read once and dropped once no future frame needs them.
        let mut input: Vec<[f32; 2]> = Vec::new();
        let mut input_base = 0_usize;
        let mut read_next = 0_usize;
        let mut frames_done = 0_usize;
        let mut written_frames = 0_usize;
        progress(0.0);

        let mut chunk_start = 0_usize;
        while chunk_start < model_frames {
            if cancel.load(Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let chunk_frames = SPLEETER_FRAMES.min(model_frames - chunk_start);

            let needed_end =
                (chunk_start + chunk_frames - 1) * SPLEETER_HOP_LENGTH + SPLEETER_N_FFT;
            fill_input_window(
                &mut reader,
                &mut input,
                input_base,
                &mut read_next,
                needed_end,
                source_frames,
            )?;
            let keep_from = chunk_start * SPLEETER_HOP_LENGTH;
            if keep_from > input_base {
                let drop = (keep_from - input_base).min(input.len());
                input.drain(..drop);
                input_base += drop;
            }

            tensor.fill(0.0);
            real.fill(0.0);
            imag.fill(0.0);
            for channel in 0..2 {
                let channel_plane = channel * plane;
                for frame in 0..chunk_frames {
                    let start = (chunk_start + frame) * SPLEETER_HOP_LENGTH;
                    spectrum.fill(Complex::new(0.0_f32, 0.0_f32));
                    for index in 0..SPLEETER_N_FFT {
                        let sample = window_sample(&input, input_base, start + index)[channel];
                        spectrum[index] = Complex::new(sample * self.window[index], 0.0);
                    }
                    self.forward.process(&mut spectrum);
                    for bin in 0..SPLEETER_BINS {
                        let value = spectrum[bin];
                        let slot = channel_plane + frame * SPLEETER_BINS + bin;
                        real[slot] = value.re;
                        imag[slot] = value.im;
                        tensor[slot] = (value.re * value.re + value.im * value.im).sqrt();
                    }
                }
            }

            let vocals_values = run_graph(
                &mut self.vocals,
                &self.input_name,
                &self.output_name,
                &tensor,
            )?;
            let accompaniment_values = run_graph(
                &mut self.accompaniment,
                &self.input_name,
                &self.output_name,
                &tensor,
            )?;

            for frame in 0..chunk_frames {
                for (stem_index, values) in [&vocals_values, &accompaniment_values]
                    .into_iter()
                    .enumerate()
                {
                    for channel in 0..2 {
                        let channel_plane = channel * plane;
                        spectrum.fill(Complex::new(0.0_f32, 0.0_f32));
                        for bin in 0..SPLEETER_BINS {
                            let slot = channel_plane + frame * SPLEETER_BINS + bin;
                            let vocals = vocals_values[slot];
                            let accompaniment = accompaniment_values[slot];
                            let vocals_energy = vocals * vocals;
                            let accompaniment_energy = accompaniment * accompaniment;
                            let total = vocals_energy + accompaniment_energy + SPLEETER_EPSILON;
                            let own = if stem_index == 0 {
                                vocals_energy
                            } else {
                                accompaniment_energy
                            };
                            let mask = if total > 0.0 {
                                (own + SPLEETER_EPSILON / 2.0) / total
                            } else {
                                0.0
                            };
                            let mask = if mask.is_finite() {
                                mask.clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            let value = Complex::new(real[slot] * mask, imag[slot] * mask);
                            spectrum[bin] = value;
                            match bin {
                                0 => {}
                                frequency if frequency < SPLEETER_N_FFT / 2 => {
                                    spectrum[SPLEETER_N_FFT - frequency] =
                                        Complex::new(value.re, -value.im);
                                }
                                _ => {}
                            }
                        }
                        self.inverse.process(&mut spectrum);
                        for index in 0..SPLEETER_N_FFT {
                            let window = self.window[index];
                            accumulators[stem_index][index][channel] +=
                                spectrum[index].re * window / SPLEETER_N_FFT as f32;
                            if stem_index == 0 && channel == 0 {
                                window_energy[index] += window * window;
                            }
                        }
                    }
                }

                for (stem_index, file) in raw_files.iter_mut().enumerate() {
                    let mut bytes = Vec::with_capacity(SPLEETER_HOP_LENGTH * 4);
                    for index in 0..SPLEETER_HOP_LENGTH {
                        let scale = window_energy[index].max(1.0e-12);
                        let left = super::separator::float_to_i16(
                            accumulators[stem_index][index][0] / scale,
                        );
                        let right = super::separator::float_to_i16(
                            accumulators[stem_index][index][1] / scale,
                        );
                        bytes.extend_from_slice(&left.to_le_bytes());
                        bytes.extend_from_slice(&right.to_le_bytes());
                    }
                    file.write_all(&bytes)?;
                }
                written_frames += SPLEETER_HOP_LENGTH;

                for stem in accumulators.iter_mut() {
                    stem.copy_within(SPLEETER_HOP_LENGTH.., 0);
                    for slot in stem.iter_mut().skip(SPLEETER_N_FFT - SPLEETER_HOP_LENGTH) {
                        *slot = [0.0_f32; 2];
                    }
                }
                window_energy.copy_within(SPLEETER_HOP_LENGTH.., 0);
                for slot in window_energy
                    .iter_mut()
                    .skip(SPLEETER_N_FFT - SPLEETER_HOP_LENGTH)
                {
                    *slot = 0.0;
                }
                frames_done += 1;
            }

            chunk_start += chunk_frames;
            let done = frames_done as f64 / model_frames.max(1) as f64;
            progress(done * 0.8);
        }

        // `center = false` framing cannot cover the final `n_fft - hop` samples of the file.
        if written_frames < source_frames {
            let missing = source_frames - written_frames;
            let silence = vec![0_u8; missing.saturating_mul(4)];
            for file in &mut raw_files {
                file.write_all(&silence)?;
            }
        }
        for file in &mut raw_files {
            file.flush()?;
        }
        drop(raw_files);

        let flac_config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| anyhow::anyhow!("invalid FLAC encoder configuration: {error}"))?;
        let block_size = flac_config.block_size;
        let encoded_frames = source_frames.max(16);
        let max_block_size = block_size.min(encoded_frames);
        anyhow::ensure!(
            max_block_size >= 16 && max_block_size <= u16::MAX as usize,
            "FLAC block size is outside the supported range"
        );
        let sample_rate = SPLEETER_SAMPLE_RATE as usize;
        let mut stream_info = flacenc::component::StreamInfo::new(sample_rate, 2, 16)
            .map_err(|error| anyhow::anyhow!("invalid FLAC stream metadata: {error}"))?;
        stream_info
            .set_block_sizes(max_block_size, max_block_size)
            .map_err(|error| anyhow::anyhow!("invalid FLAC block-size metadata: {error}"))?;
        stream_info.set_total_samples(encoded_frames);

        let mut outputs = Vec::with_capacity(SPLEETER_STEM_NAMES.len());
        for (index, name) in SPLEETER_STEM_NAMES.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let output_path = output_dir.join(format!("{name}.flac"));
            super::separator::encode_flac_streaming(
                &raw_paths[index],
                &output_path,
                source_frames,
                encoded_frames,
                sample_rate,
                block_size,
                &flac_config,
                &stream_info,
                cancel,
            )
            .with_context(|| format!("could not encode {name} stem as FLAC"))?;
            fs::remove_file(&raw_paths[index]).ok();
            outputs.push(output_path);
            progress(0.8 + 0.2 * (index + 1) as f64 / SPLEETER_STEM_NAMES.len() as f64);
        }
        Ok(outputs)
    }
}

fn validate_spleeter_session(session: &Session) -> Result<()> {
    anyhow::ensure!(
        session.inputs().len() == 1 && session.outputs().len() == 1,
        "Spleeter graphs must have exactly one input and one output tensor"
    );
    let input = session
        .inputs()
        .first()
        .context("Spleeter graph has no input")?;
    let output = session
        .outputs()
        .first()
        .context("Spleeter graph has no output")?;
    let input_shape =
        float_tensor_shape(input.dtype()).context("Spleeter input tensor must be float32")?;
    let output_shape =
        float_tensor_shape(output.dtype()).context("Spleeter output tensor must be float32")?;
    anyhow::ensure!(
        crate::audio::model_loader::spleeter_input_shape_matches(&input_shape),
        "Spleeter input must be float32 [2, splits, {SPLEETER_FRAMES}, {SPLEETER_BINS}], found {input_shape:?}"
    );
    anyhow::ensure!(
        crate::audio::model_loader::spleeter_output_shape_matches(&output_shape),
        "Spleeter output must be float32 [2, splits, {SPLEETER_FRAMES}, {SPLEETER_BINS}], found {output_shape:?}"
    );
    Ok(())
}

fn float_tensor_shape(value_type: &ValueType) -> Result<Vec<i64>> {
    match value_type {
        ValueType::Tensor { ty, shape, .. } => {
            anyhow::ensure!(*ty == TensorElementType::Float32, "tensor is not float32");
            Ok(shape.iter().copied().collect())
        }
        _ => anyhow::bail!("ONNX value must be a tensor"),
    }
}

fn run_graph(
    session: &mut Session,
    input_name: &str,
    output_name: &str,
    input: &[f32],
) -> Result<Vec<f32>> {
    let tensor = Tensor::from_array((
        [2_usize, 1_usize, SPLEETER_FRAMES, SPLEETER_BINS],
        input.to_vec().into_boxed_slice(),
    ))?;
    let outputs = session
        .run(ort::inputs![input_name => tensor])
        .context("Spleeter ONNX inference failed")?;
    let output = outputs
        .get(output_name)
        .context("Spleeter output tensor is missing")?;
    let (shape, values) = output
        .try_extract_tensor::<f32>()
        .context("Spleeter output is not float32")?;
    anyhow::ensure!(
        shape.len() == 4
            && shape[0] == 2
            && shape[2] == SPLEETER_FRAMES as i64
            && shape[3] == SPLEETER_BINS as i64,
        "Spleeter produced an unexpected output shape {shape:?}"
    );
    anyhow::ensure!(
        values.len() == 2 * SPLEETER_FRAMES * SPLEETER_BINS,
        "Spleeter produced an unexpected output element count"
    );
    anyhow::ensure!(
        values.iter().all(|value| value.is_finite()),
        "Spleeter produced a non-finite value"
    );
    Ok(values.to_vec())
}

fn fill_input_window(
    reader: &mut WavReader<BufReader<File>>,
    input: &mut Vec<[f32; 2]>,
    input_base: usize,
    read_next: &mut usize,
    needed_end: usize,
    total_frames: usize,
) -> Result<()> {
    while input_base + input.len() < needed_end && *read_next < total_frames {
        let left = reader.read_sample::<i16>().with_context(|| {
            format!("normalized audio ended after {read_next} of {total_frames} frames")
        })?;
        let right = reader.read_sample::<i16>().with_context(|| {
            format!("normalized audio ended after {read_next} of {total_frames} frames")
        })?;
        input.push([
            super::separator::i16_to_f32(left),
            super::separator::i16_to_f32(right),
        ]);
        *read_next += 1;
    }
    Ok(())
}

fn window_sample(input: &[[f32; 2]], input_base: usize, index: usize) -> [f32; 2] {
    let Some(offset) = index.checked_sub(input_base) else {
        return [0.0, 0.0];
    };
    input.get(offset).copied().unwrap_or([0.0, 0.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_names_follow_spleeter_stem_order() {
        assert_eq!(SPLEETER_STEM_NAMES, ["acapella", "instrumental"]);
    }

    #[test]
    fn sliding_window_zero_fills_out_of_range_samples() {
        let input = vec![[0.5_f32, -0.5], [0.25, -0.25]];
        assert_eq!(window_sample(&input, 10, 10), [0.5, -0.5]);
        assert_eq!(window_sample(&input, 10, 11), [0.25, -0.25]);
        assert_eq!(window_sample(&input, 10, 12), [0.0, 0.0]);
        assert_eq!(window_sample(&input, 10, 9), [0.0, 0.0]);
    }

    #[test]
    fn ratio_mask_sums_to_one_and_stays_finite() {
        let vocals_energy = 4.0_f32;
        let accompaniment_energy = 1.0_f32;
        let total = vocals_energy + accompaniment_energy + SPLEETER_EPSILON;
        let vocals_mask = (vocals_energy + SPLEETER_EPSILON / 2.0) / total;
        let accompaniment_mask = (accompaniment_energy + SPLEETER_EPSILON / 2.0) / total;
        assert!((vocals_mask + accompaniment_mask - 1.0).abs() < 1.0e-6);
        assert!((0.0..=1.0).contains(&vocals_mask));
        assert!((0.0..=1.0).contains(&accompaniment_mask));
    }

    #[test]
    fn frame_count_matches_center_false_framing() {
        // 44_100 samples of audio at hop 1024 with a 4096 window.
        let source_frames = 44_100_usize;
        let model_frames = 1 + (source_frames - SPLEETER_N_FFT) / SPLEETER_HOP_LENGTH;
        let covered = model_frames * SPLEETER_HOP_LENGTH;
        assert!(covered <= source_frames);
        assert!(source_frames - covered < SPLEETER_N_FFT);
    }
}
