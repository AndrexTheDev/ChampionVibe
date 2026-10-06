//! CPU-first MDX-Net inference for the pinned KUIELab Track A single-target ONNX models.
//! The model contract is deliberately explicit in `.cvmodel.json`; arbitrary ONNX files are not
//! guessed from their extension and cannot enter this path without validating their tensor graph.

use crate::audio::model_manager::{
    read_manifest, validate_manifest_models, SeparatorModelEntry, SeparatorModelManifest,
};
use anyhow::{Context, Result};
use flacenc::error::Verify;
use hound::WavReader;
use ort::{session::Session, value::Tensor};
use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const SAMPLE_RATE: u32 = 44_100;
const DIM_F: usize = 2_048;
const HOP_LENGTH: usize = 1_024;
const MDX_CHANNELS: usize = 4;
const MDX_FRAMES: usize = 512;
const OUTPUT_STEMS: [&str; 4] = ["acapella", "instrumental", "bass", "drums"];

pub(crate) struct MdxSeparator {
    mode: String,
    targets: Vec<MdxTargetConfig>,
    active_model: Option<MdxTarget>,
    threads: usize,
    use_directml: bool,
    backend: &'static str,
}

struct MdxTargetConfig {
    entry: SeparatorModelEntry,
    model_path: PathBuf,
}

struct MdxTarget {
    entry: SeparatorModelEntry,
    session: Session,
    input_name: String,
    output_name: String,
    dsp: MdxDsp,
    backend: &'static str,
}

struct MdxDsp {
    n_fft: usize,
    frames: usize,
    hop_length: usize,
    window: Vec<f32>,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl MdxSeparator {
    pub(crate) fn load(path: &Path, threads: usize, use_directml: bool) -> Result<Self> {
        anyhow::ensure!(
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".cvmodel.json")),
            "MDX-Net requires a ChampionVibe .cvmodel.json model manifest"
        );
        let manifest = read_manifest(path)?;
        let directory = path
            .parent()
            .context("model manifest has no parent directory")?;
        let resolved = validate_manifest_models(&manifest, directory, true)?;
        let targets = resolved
            .into_iter()
            .map(|(entry, model_path)| MdxTargetConfig { entry, model_path })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !targets.is_empty(),
            "MDX-Net manifest contains no inference targets"
        );
        let threads = threads.clamp(1, 8);
        // Keep one ONNX session at a time. A four-target profile should remain usable on
        // memory-constrained Android devices without retaining all four weight graphs.
        let active_model = Some(load_target(&targets[0], threads, use_directml)?);
        let backend = active_model
            .as_ref()
            .map(|model| model.backend)
            .unwrap_or("CPU");
        Ok(Self {
            mode: manifest.mode,
            targets,
            active_model,
            threads,
            use_directml,
            backend,
        })
    }

    pub(crate) fn backend(&self) -> &'static str {
        self.backend
    }

    pub(crate) fn stem_count(&self) -> usize {
        if self.mode == "two-stem" {
            2
        } else {
            4
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn separate(
        &mut self,
        input_path: &Path,
        output_dir: &Path,
        total_frames: u64,
        cancel: &std::sync::atomic::AtomicBool,
        mut progress: impl FnMut(f64),
    ) -> Result<Vec<PathBuf>> {
        anyhow::ensure!(total_frames > 0, "audio source contains no frames");
        let total_frames =
            usize::try_from(total_frames).context("audio source is too long for this platform")?;
        fs::create_dir_all(output_dir).with_context(|| {
            format!(
                "cannot create MDX-Net output folder {}",
                output_dir.display()
            )
        })?;

        let raw_paths = OUTPUT_STEMS
            .iter()
            .map(|stem| output_dir.join(format!(".{stem}.mdx-pcm")))
            .collect::<Vec<_>>();
        let raw_count = self.stem_count();
        let mut raw_outputs = raw_paths
            .iter()
            .take(raw_count)
            .map(|path| {
                File::create(path)
                    .with_context(|| {
                        format!("cannot create temporary MDX-Net output {}", path.display())
                    })
                    .map(BufWriter::new)
            })
            .collect::<Result<Vec<_>>>()?;

        let target_count = self.targets.len();
        for target_index in 0..target_count {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let target_stem = self.targets[target_index].entry.stem.clone();
            let mut model = match self.active_model.take() {
                Some(model) if model.entry.stem == target_stem => model,
                Some(model) => {
                    drop(model);
                    load_target(&self.targets[target_index], self.threads, self.use_directml)?
                }
                None => load_target(&self.targets[target_index], self.threads, self.use_directml)?,
            };
            if self.mode == "two-stem" {
                anyhow::ensure!(
                    target_stem == "vocals",
                    "two-stem MDX profile must contain only a vocals target"
                );
                let (vocals, instrumental) = raw_outputs.split_at_mut(1);
                let vocal_writer = vocals
                    .first_mut()
                    .context("vocal output writer is missing")?;
                let instrumental_writer = instrumental
                    .first_mut()
                    .context("instrumental output writer is missing")?;
                infer_target(
                    &mut model,
                    input_path,
                    total_frames,
                    vocal_writer,
                    Some(instrumental_writer),
                    cancel,
                    |within_target| progress(within_target * 0.9),
                )?;
            } else {
                let output_index = match target_stem.as_str() {
                    "vocals" => 0,
                    "other" => 1,
                    "bass" => 2,
                    "drums" => 3,
                    _ => anyhow::bail!("unsupported MDX-Net target {target_stem}"),
                };
                let output = raw_outputs
                    .get_mut(output_index)
                    .context("MDX-Net output writer is missing")?;
                infer_target(
                    &mut model,
                    input_path,
                    total_frames,
                    output,
                    None,
                    cancel,
                    |within_target| {
                        progress(0.9 * (target_index as f64 + within_target) / target_count as f64)
                    },
                )?;
            }
            // `model` drops here, releasing its ONNX weights before the next target loads.
        }
        for output in &mut raw_outputs {
            output.flush()?;
        }
        drop(raw_outputs);

        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| anyhow::anyhow!("invalid FLAC encoder configuration: {error}"))?;
        let block_size = config.block_size;
        let encoded_frames = total_frames.max(16);
        let max_block_size = block_size.min(encoded_frames);
        anyhow::ensure!(
            max_block_size >= 16 && max_block_size <= u16::MAX as usize,
            "FLAC block size is outside the supported range"
        );
        let mut stream_info = flacenc::component::StreamInfo::new(SAMPLE_RATE as usize, 2, 16)
            .map_err(|error| anyhow::anyhow!("invalid FLAC stream metadata: {error}"))?;
        stream_info
            .set_block_sizes(max_block_size, max_block_size)
            .map_err(|error| anyhow::anyhow!("invalid FLAC block-size metadata: {error}"))?;
        stream_info.set_total_samples(encoded_frames);

        let mut outputs = Vec::with_capacity(raw_count);
        for (index, stem) in OUTPUT_STEMS.iter().take(raw_count).enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let output_path = output_dir.join(format!("{stem}.flac"));
            super::separator::encode_flac_streaming(
                &raw_paths[index],
                &output_path,
                total_frames,
                encoded_frames,
                SAMPLE_RATE as usize,
                block_size,
                &config,
                &stream_info,
                cancel,
            )
            .with_context(|| format!("could not encode MDX-Net {stem} stem as FLAC"))?;
            fs::remove_file(&raw_paths[index]).ok();
            outputs.push(output_path);
            progress(0.95 + 0.05 * (index + 1) as f64 / raw_count as f64);
        }
        Ok(outputs)
    }
}

fn load_target(config: &MdxTargetConfig, threads: usize, use_directml: bool) -> Result<MdxTarget> {
    let (session, backend) =
        create_mdx_session(&config.model_path, threads.clamp(1, 8), use_directml)?;
    let input = session
        .inputs()
        .first()
        .context("MDX-Net ONNX model has no input")?;
    let output = session
        .outputs()
        .first()
        .context("MDX-Net ONNX model has no output")?;
    anyhow::ensure!(
        input.name() == "input",
        "MDX-Net input tensor must be named 'input'"
    );
    let input_name = input.name().to_owned();
    let output_name = output.name().to_owned();
    let dsp = MdxDsp::new(&config.entry)?;
    Ok(MdxTarget {
        entry: config.entry.clone(),
        session,
        input_name,
        output_name,
        dsp,
        backend,
    })
}

fn infer_target(
    model: &mut MdxTarget,
    input_path: &Path,
    total_frames: usize,
    vocals_output: &mut BufWriter<File>,
    mut instrumental_output: Option<&mut BufWriter<File>>,
    cancel: &std::sync::atomic::AtomicBool,
    mut progress: impl FnMut(f64),
) -> Result<()> {
    let n_fft = model.entry.n_fft;
    let trim = n_fft / 2;
    let chunk_size = HOP_LENGTH * (model.entry.frames - 1);
    let generated_per_chunk = chunk_size
        .checked_sub(n_fft)
        .context("MDX-Net FFT size exceeds the audio chunk")?;
    anyhow::ensure!(generated_per_chunk > 0, "MDX-Net generated chunk is empty");
    let chunks = total_frames.div_ceil(generated_per_chunk);
    let mut reader = WavReader::open(input_path).with_context(|| {
        format!(
            "cannot open normalized MDX-Net input {}",
            input_path.display()
        )
    })?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.channels == 2
            && spec.sample_rate == SAMPLE_RATE
            && spec.sample_format == hound::SampleFormat::Int
            && spec.bits_per_sample == 16,
        "MDX-Net input must be normalized 44.1 kHz, stereo, 16-bit PCM"
    );
    anyhow::ensure!(
        u64::from(reader.duration()) >= total_frames as u64,
        "normalized WAV is shorter than the decoded frame count"
    );
    let mut samples = reader.samples::<i16>();
    let mut frames_read = 0_usize;
    let mut chunk = vec![[0.0_f32; 2]; chunk_size];
    fill_initial_chunk(
        &mut chunk,
        trim,
        &mut samples,
        &mut frames_read,
        total_frames,
    )?;

    for chunk_index in 0..chunks {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("import cancelled");
        }
        let prediction = model.infer_chunk(&chunk)?;
        let output_start = chunk_index * generated_per_chunk;
        let output_frames = generated_per_chunk.min(total_frames - output_start);
        let compensation = model.entry.compensation;
        for offset in 0..output_frames {
            let position = trim + offset;
            let vocal = [
                prediction[position][0] * compensation,
                prediction[position][1] * compensation,
            ];
            write_stereo_frame(vocals_output, vocal)?;
            if let Some(instrumental) = instrumental_output.as_deref_mut() {
                let mix = chunk[position];
                write_stereo_frame(instrumental, [mix[0] - vocal[0], mix[1] - vocal[1]])?;
            }
        }
        progress((chunk_index + 1) as f64 / chunks as f64);
        if chunk_index + 1 < chunks {
            chunk.copy_within(generated_per_chunk.., 0);
            let fill_start = chunk.len() - generated_per_chunk;
            fill_chunk_tail(
                &mut chunk,
                fill_start,
                &mut samples,
                &mut frames_read,
                total_frames,
            )?;
        }
    }
    anyhow::ensure!(
        frames_read == total_frames,
        "MDX-Net did not consume the normalized input timeline"
    );
    Ok(())
}

impl MdxTarget {
    fn infer_chunk(&mut self, audio: &[[f32; 2]]) -> Result<Vec<[f32; 2]>> {
        let input_values = self.dsp.stft(audio);
        let input_tensor = Tensor::from_array((
            [1_usize, MDX_CHANNELS, DIM_F, self.entry.frames],
            input_values.into_boxed_slice(),
        ))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input_tensor])
            .context("MDX-Net CPU/DirectML ONNX inference failed")?;
        let output = outputs
            .get(self.output_name.as_str())
            .context("MDX-Net output tensor is missing")?;
        let (shape, values) = output
            .try_extract_tensor::<f32>()
            .context("MDX-Net output is not float32")?;
        anyhow::ensure!(
            **shape
                == [
                    1_i64,
                    MDX_CHANNELS as i64,
                    DIM_F as i64,
                    self.entry.frames as i64
                ],
            "MDX-Net output tensor must be [1, 4, 2048, {}]",
            self.entry.frames
        );
        anyhow::ensure!(
            values.iter().all(|value| value.is_finite()),
            "MDX-Net produced a non-finite output value"
        );
        Ok(self.dsp.istft(values))
    }
}

impl MdxDsp {
    fn new(entry: &SeparatorModelEntry) -> Result<Self> {
        anyhow::ensure!(entry.dim_f == DIM_F, "MDX-Net dim_f must equal 2048");
        anyhow::ensure!(
            entry.frames == MDX_FRAMES,
            "only 512-frame MDX-Net models are supported by this inference path"
        );
        anyhow::ensure!(
            entry.hop_length == HOP_LENGTH,
            "MDX-Net hop length must equal 1024 samples"
        );
        anyhow::ensure!(
            matches!(entry.n_fft, 4_096 | 6_144 | 8_192 | 16_384),
            "unsupported MDX-Net FFT size"
        );
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(entry.n_fft);
        let inverse = planner.plan_fft_inverse(entry.n_fft);
        let window = (0..entry.n_fft)
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * index as f32 / entry.n_fft as f32;
                0.5 * (1.0 - phase.cos())
            })
            .collect();
        Ok(Self {
            n_fft: entry.n_fft,
            frames: entry.frames,
            hop_length: entry.hop_length,
            window,
            forward,
            inverse,
        })
    }

    /// KUIELab's reference preprocessor uses PyTorch STFT with a periodic Hann window and then
    /// reshapes [left/right, real/imag] into the model's [1,4,2048,512] float32 tensor.
    fn stft(&self, audio: &[[f32; 2]]) -> Vec<f32> {
        let plane = DIM_F * self.frames;
        let mut tensor = vec![0.0_f32; MDX_CHANNELS * plane];
        let mut spectrum = vec![Complex::new(0.0_f32, 0.0_f32); self.n_fft];
        let center_offset = self.n_fft / 2;
        for channel in 0..2 {
            for frame in 0..self.frames {
                let center = frame * self.hop_length;
                for index in 0..self.n_fft {
                    let source_index = center as isize + index as isize - center_offset as isize;
                    // torch.stft(center=True) defaults to reflect padding. The context margins
                    // are discarded after iSTFT, but matching its boundary rule avoids a needless
                    // preprocessing mismatch at chunk edges.
                    let reflected = reflect_index(source_index, audio.len());
                    let sample = audio[reflected][channel];
                    spectrum[index] = Complex::new(sample * self.window[index], 0.0);
                }
                self.forward.process(&mut spectrum);
                let real_plane = channel * 2 * plane;
                let imag_plane = real_plane + plane;
                for frequency in 0..DIM_F {
                    let index = frequency * self.frames + frame;
                    tensor[real_plane + index] = spectrum[frequency].re;
                    tensor[imag_plane + index] = spectrum[frequency].im;
                }
            }
        }
        tensor
    }

    /// Reconstructs a stereo waveform using the same one-sided-spectrum zero-fill and centered
    /// Hann iSTFT as KUIELab's Python reference. High bins omitted by dim_f are zeroed.
    fn istft(&self, values: &[f32]) -> Vec<[f32; 2]> {
        let padded_frames = (self.frames - 1) * self.hop_length + self.n_fft;
        let mut output = vec![[0.0_f32; 2]; padded_frames];
        let mut normalization = vec![0.0_f32; padded_frames];
        let plane = DIM_F * self.frames;
        let mut spectrum = vec![Complex::new(0.0_f32, 0.0_f32); self.n_fft];
        for frame in 0..self.frames {
            let frame_start = frame * self.hop_length;
            for channel in 0..2 {
                spectrum.fill(Complex::new(0.0, 0.0));
                let real_plane = channel * 2 * plane;
                let imag_plane = real_plane + plane;
                for frequency in 0..DIM_F {
                    let index = frequency * self.frames + frame;
                    let value =
                        Complex::new(values[real_plane + index], values[imag_plane + index]);
                    spectrum[frequency] = value;
                    if frequency > 0 && frequency < self.n_fft / 2 {
                        spectrum[self.n_fft - frequency] = Complex::new(value.re, -value.im);
                    }
                }
                self.inverse.process(&mut spectrum);
                for index in 0..self.n_fft {
                    let output_index = frame_start + index;
                    let window = self.window[index];
                    output[output_index][channel] +=
                        spectrum[index].re * window / self.n_fft as f32;
                    if channel == 0 {
                        normalization[output_index] += window * window;
                    }
                }
            }
        }
        let crop_start = self.n_fft / 2;
        let output_length = (self.frames - 1) * self.hop_length;
        let mut cropped = Vec::with_capacity(output_length);
        for index in crop_start..crop_start + output_length {
            let scale = normalization[index].max(1.0e-12);
            cropped.push([output[index][0] / scale, output[index][1] / scale]);
        }
        cropped
    }
}

fn reflect_index(index: isize, length: usize) -> usize {
    debug_assert!(length > 1);
    let length = length as isize;
    let reflected = if index < 0 {
        -index
    } else if index >= length {
        2 * length - 2 - index
    } else {
        index
    };
    reflected.clamp(0, length - 1) as usize
}

fn fill_initial_chunk(
    chunk: &mut [[f32; 2]],
    zero_prefix: usize,
    samples: &mut impl Iterator<Item = std::result::Result<i16, hound::Error>>,
    frames_read: &mut usize,
    total_frames: usize,
) -> Result<()> {
    for frame in chunk.iter_mut().skip(zero_prefix) {
        *frame = read_source_frame(samples, frames_read, total_frames)?;
    }
    Ok(())
}

fn fill_chunk_tail(
    chunk: &mut [[f32; 2]],
    overlap_frames: usize,
    samples: &mut impl Iterator<Item = std::result::Result<i16, hound::Error>>,
    frames_read: &mut usize,
    total_frames: usize,
) -> Result<()> {
    for frame in chunk.iter_mut().skip(overlap_frames) {
        *frame = read_source_frame(samples, frames_read, total_frames)?;
    }
    Ok(())
}

fn read_source_frame(
    samples: &mut impl Iterator<Item = std::result::Result<i16, hound::Error>>,
    frames_read: &mut usize,
    total_frames: usize,
) -> Result<[f32; 2]> {
    if *frames_read >= total_frames {
        return Ok([0.0; 2]);
    }
    let left = samples
        .next()
        .context("normalized WAV ended before its declared duration")??;
    let right = samples
        .next()
        .context("normalized WAV has an incomplete stereo frame")??;
    *frames_read += 1;
    Ok([f32::from(left) / 32_768.0, f32::from(right) / 32_768.0])
}

fn write_stereo_frame(writer: &mut impl Write, frame: [f32; 2]) -> Result<()> {
    for sample in frame {
        anyhow::ensure!(
            sample.is_finite(),
            "MDX-Net produced a non-finite audio sample"
        );
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm = if clamped <= -1.0 {
            i16::MIN
        } else {
            (clamped * i16::MAX as f32).round() as i16
        };
        writer.write_all(&pcm.to_le_bytes())?;
    }
    Ok(())
}

fn create_mdx_session(
    path: &Path,
    threads: usize,
    use_directml: bool,
) -> Result<(Session, &'static str)> {
    #[cfg(windows)]
    if use_directml {
        let directml = Session::builder()
            .and_then(|builder| {
                builder.with_execution_providers([ort::ep::DirectML::default().build()])
            })
            .and_then(|builder| builder.with_intra_threads(threads))
            .and_then(|builder| builder.with_inter_threads(1))
            .and_then(|builder| builder.with_parallel_execution(false))
            .and_then(|builder| builder.commit_from_file(path));
        if let Ok(session) = directml {
            return Ok((session, "DirectML"));
        }
    }
    #[cfg(not(windows))]
    let _ = use_directml;

    let session = Session::builder()
        .map_err(|error| anyhow::anyhow!("could not create ONNX session builder: {error}"))?
        .with_intra_threads(threads)
        .map_err(|error| anyhow::anyhow!("could not set ONNX intra-op threads: {error}"))?
        .with_inter_threads(1)
        .map_err(|error| anyhow::anyhow!("could not set ONNX inter-op threads: {error}"))?
        .with_parallel_execution(false)
        .map_err(|error| anyhow::anyhow!("could not disable ONNX parallel execution: {error}"))?
        .commit_from_file(path)
        .map_err(|error| {
            anyhow::anyhow!(
                "could not load local MDX-Net ONNX model {}: {error}",
                path.display()
            )
        })?;
    Ok((session, "CPU"))
}

#[cfg(test)]
mod tests {
    use super::{MdxDsp, DIM_F, HOP_LENGTH};
    use crate::audio::model_manager::SeparatorModelEntry;
    use rustfft::{num_complex::Complex, FftPlanner};

    fn entry(n_fft: usize) -> SeparatorModelEntry {
        SeparatorModelEntry {
            stem: "vocals".to_owned(),
            file: "vocals.onnx".to_owned(),
            sha256: None,
            n_fft,
            dim_f: DIM_F,
            frames: 512,
            hop_length: HOP_LENGTH,
            compensation: 1.0,
        }
    }

    #[test]
    fn mdx_stft_uses_the_declared_channel_and_frequency_layout() {
        let dsp = MdxDsp::new(&entry(4_096)).unwrap();
        let mut audio = vec![[0.0_f32; 2]; HOP_LENGTH * 511];
        audio[300][0] = 1.0;
        audio[300][1] = -0.5;
        let input = dsp.stft(&audio);
        assert_eq!(input.len(), 4 * DIM_F * 512);
        assert!(input.iter().all(|sample| sample.is_finite()));
        assert!(input[..DIM_F * 512].iter().any(|sample| sample.abs() > 0.0));
        assert!(input[2 * DIM_F * 512..]
            .iter()
            .any(|sample| sample.abs() > 0.0));
    }

    #[test]
    fn centered_hann_stft_roundtrips_a_sine_below_dim_f_cutoff() {
        let dsp = MdxDsp::new(&entry(4_096)).unwrap();
        let length = HOP_LENGTH * 511;
        let frequency = 440.0_f32;
        let audio = (0..length)
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * frequency * index as f32 / 44_100.0;
                [0.25 * phase.sin(), -0.125 * phase.sin()]
            })
            .collect::<Vec<_>>();
        let packed = dsp.stft(&audio);
        let restored = dsp.istft(&packed);
        let offset = 2_048;
        let error = (offset..length - offset)
            .map(|index| (restored[index][0] - audio[index][0]).abs())
            .sum::<f32>()
            / (length - 2 * offset) as f32;
        assert!(error < 0.002, "mean reconstruction error was {error}");
    }

    #[test]
    fn periodic_hann_window_and_inverse_scale_match_expected_fft_convention() {
        let dsp = MdxDsp::new(&entry(4_096)).unwrap();
        assert!(dsp.window[0].abs() < 1.0e-6);
        assert!((dsp.window[2_048] - 1.0).abs() < 1.0e-6);
        let mut planner = FftPlanner::<f32>::new();
        let inverse = planner.plan_fft_inverse(4_096);
        let mut impulse = vec![Complex::new(0.0_f32, 0.0_f32); 4_096];
        impulse[0].re = 1.0;
        inverse.process(&mut impulse);
        assert!((impulse[0].re / 4_096.0 - 1.0 / 4_096.0).abs() < 1.0e-7);
    }
}
