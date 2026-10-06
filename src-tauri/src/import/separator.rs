use super::mdx::MdxSeparator;
use anyhow::{Context, Result};
use flacenc::{
    bitsink::ByteSink,
    component::{BitRepr, StreamInfo},
    error::{SourceError, Verify},
    source::{Fill, FrameBuf, Source},
};
use hound::WavReader;
use ort::{
    session::Session,
    value::{Tensor, TensorElementType, ValueType},
};
use std::{
    fs::{self, File},
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) const DEMUCS_WINDOW_SAMPLES: usize = 343_980;
// HT-Demucs' 25% overlap means each chunk advances by 75% of its window.
const HOP_SAMPLES: usize = DEMUCS_WINDOW_SAMPLES * 3 / 4;
const OVERLAP_WEIGHT_FLOOR: f32 = 0.1;
const CHAMPION_STEM_NAMES: [&str; 4] = ["acapella", "instrumental", "bass", "drums"];
// HT-Demucs emits [drums, bass, other, vocals]. ChampionVibe stores
// [acapella, instrumental, bass, drums].
const SOURCE_INDEX_FOR_CHAMPION_STEM: [usize; 4] = [3, 2, 1, 0];

pub(crate) enum SeparationEngine {
    HtDemucs(LocalSeparator),
    Mdx(MdxSeparator),
}

impl SeparationEngine {
    pub(crate) fn load(model_path: &Path, threads: usize, use_directml: bool) -> Result<Self> {
        let is_mdx_manifest = model_path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".cvmodel.json"));
        if is_mdx_manifest {
            Ok(Self::Mdx(MdxSeparator::load(
                model_path,
                threads,
                use_directml,
            )?))
        } else {
            Ok(Self::HtDemucs(LocalSeparator::load(
                model_path,
                threads,
                use_directml,
            )?))
        }
    }

    pub(crate) fn backend(&self) -> &'static str {
        match self {
            Self::HtDemucs(separator) => separator.backend(),
            Self::Mdx(separator) => separator.backend(),
        }
    }

    pub(crate) fn stem_count(&self) -> usize {
        match self {
            Self::HtDemucs(_) => 4,
            Self::Mdx(separator) => separator.stem_count(),
        }
    }

    pub(crate) fn separate(
        &mut self,
        input_path: &Path,
        output_dir: &Path,
        total_frames: u64,
        cancel: &AtomicBool,
        progress: impl FnMut(f64),
    ) -> Result<Vec<PathBuf>> {
        match self {
            Self::HtDemucs(separator) => {
                separator.separate(input_path, output_dir, total_frames, cancel, progress)
            }
            Self::Mdx(separator) => {
                separator.separate(input_path, output_dir, total_frames, cancel, progress)
            }
        }
    }
}

pub(crate) struct LocalSeparator {
    session: Session,
    input_name: String,
    output_name: String,
    backend: &'static str,
}

impl LocalSeparator {
    pub(crate) fn load(model_path: &Path, threads: usize, use_directml: bool) -> Result<Self> {
        anyhow::ensure!(
            model_path.is_file(),
            "HT-Demucs ONNX model does not exist: {}",
            model_path.display()
        );
        let threads = threads.clamp(1, 8);
        let (session, backend) = create_session(model_path, threads, use_directml)?;
        validate_htdemucs_session(&session)?;
        let input_name = session
            .inputs()
            .first()
            .context("separator ONNX model has no inputs")?
            .name()
            .to_owned();
        let output_name = session
            .outputs()
            .first()
            .context("separator ONNX model has no outputs")?
            .name()
            .to_owned();
        anyhow::ensure!(!input_name.is_empty(), "separator ONNX input name is empty");
        anyhow::ensure!(
            !output_name.is_empty(),
            "separator ONNX output name is empty"
        );
        Ok(Self {
            session,
            input_name,
            output_name,
            backend,
        })
    }

    pub(crate) fn backend(&self) -> &'static str {
        self.backend
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
                && spec.sample_rate == 44_100
                && spec.sample_format == hound::SampleFormat::Int
                && spec.bits_per_sample == 16,
            "separator input must be normalized 44.1 kHz, stereo, 16-bit PCM"
        );
        let declared_frames = u64::from(reader.duration());
        anyhow::ensure!(
            declared_frames >= total_frames,
            "normalized WAV is shorter than the decoded frame count"
        );
        let mut samples = reader.samples::<i16>();

        let raw_paths: Vec<PathBuf> = CHAMPION_STEM_NAMES
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

        let mut window = vec![[0.0_f32; 2]; DEMUCS_WINDOW_SAMPLES];
        let mut frames_read = 0_u64;
        fill_window(&mut window, &mut samples, &mut frames_read, total_frames)?;
        let mut emitted_frames = 0_u64;
        let mut previous_tail: Option<Vec<[[f32; 2]; 4]>> = None;
        let overlap_frames = DEMUCS_WINDOW_SAMPLES - HOP_SAMPLES;
        progress(0.0);

        while emitted_frames < total_frames {
            if cancel.load(Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let separated = self.infer_window(&window)?;
            if emitted_frames == 0 {
                let count = HOP_SAMPLES.min(source_frames - emitted_frames as usize);
                for frame in separated.iter().take(count) {
                    write_raw_frame(&mut raw_files, frame)?;
                }
                emitted_frames += count as u64;
                previous_tail = Some(separated[HOP_SAMPLES..].to_vec());
            } else {
                let previous = previous_tail
                    .as_ref()
                    .context("separator overlap state is missing")?;
                anyhow::ensure!(
                    previous.len() == overlap_frames,
                    "separator overlap window is invalid"
                );
                let count = HOP_SAMPLES.min(source_frames - emitted_frames as usize);
                let mut frames_output = 0_usize;
                for index in 0..overlap_frames.min(count) {
                    let old_weight = overlap_weight(HOP_SAMPLES + index);
                    let new_weight = overlap_weight(index);
                    let weight_sum = (old_weight + new_weight).max(1.0e-6);
                    let mut mixed = [[0.0_f32; 2]; 4];
                    for stem in 0..4 {
                        for channel in 0..2 {
                            mixed[stem][channel] = (previous[index][stem][channel] * old_weight
                                + separated[index][stem][channel] * new_weight)
                                / weight_sum;
                        }
                    }
                    write_raw_frame(&mut raw_files, &mixed)?;
                    frames_output += 1;
                }
                let body_end =
                    HOP_SAMPLES.min(overlap_frames + count.saturating_sub(frames_output));
                for frame in separated.iter().take(body_end).skip(overlap_frames) {
                    if frames_output >= count {
                        break;
                    }
                    write_raw_frame(&mut raw_files, frame)?;
                    frames_output += 1;
                }
                anyhow::ensure!(
                    frames_output == count,
                    "separator did not emit the requested frame count"
                );
                emitted_frames += count as u64;
                previous_tail = Some(separated[HOP_SAMPLES..].to_vec());
            }
            progress((emitted_frames as f64 / total_frames as f64) * 0.8);
            if emitted_frames >= total_frames {
                break;
            }
            window.copy_within(HOP_SAMPLES.., 0);
            fill_window_tail(&mut window, &mut samples, &mut frames_read, total_frames)?;
        }

        for file in &mut raw_files {
            file.flush()?;
        }
        drop(raw_files);

        let flac_config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| anyhow::anyhow!("invalid FLAC encoder configuration: {error}"))?;
        let block_size = flac_config.block_size;
        // FLAC frames shorter than 16 samples are invalid; pad only pathological
        // sub-millisecond clips with silence while preserving the stem timeline.
        let encoded_frames = source_frames.max(16);
        let max_block_size = block_size.min(encoded_frames);
        anyhow::ensure!(
            max_block_size >= 16 && max_block_size <= u16::MAX as usize,
            "FLAC block size is outside the supported range"
        );
        let mut stream_info = StreamInfo::new(44_100, 2, 16)
            .map_err(|error| anyhow::anyhow!("invalid FLAC stream metadata: {error}"))?;
        stream_info
            .set_block_sizes(max_block_size, max_block_size)
            .map_err(|error| anyhow::anyhow!("invalid FLAC block-size metadata: {error}"))?;
        stream_info.set_total_samples(encoded_frames);
        let mut outputs = Vec::with_capacity(4);
        for (index, name) in CHAMPION_STEM_NAMES.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                anyhow::bail!("import cancelled");
            }
            let output_path = output_dir.join(format!("{name}.flac"));
            encode_flac_streaming(
                &raw_paths[index],
                &output_path,
                source_frames,
                encoded_frames,
                44_100,
                block_size,
                &flac_config,
                &stream_info,
                cancel,
            )
            .with_context(|| format!("could not encode {name} stem as FLAC"))?;
            fs::remove_file(&raw_paths[index]).ok();
            outputs.push(output_path);
            progress(0.8 + 0.2 * (index + 1) as f64 / CHAMPION_STEM_NAMES.len() as f64);
        }
        Ok(outputs)
    }

    fn infer_window(&mut self, window: &[[f32; 2]]) -> Result<Vec<[[f32; 2]; 4]>> {
        let mut planar = Vec::with_capacity(DEMUCS_WINDOW_SAMPLES * 2);
        planar.extend(window.iter().map(|frame| frame[0]));
        planar.extend(window.iter().map(|frame| frame[1]));
        let tensor = Tensor::from_array((
            [1_usize, 2, DEMUCS_WINDOW_SAMPLES],
            planar.into_boxed_slice(),
        ))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .context("HT-Demucs ONNX inference failed")?;
        let output = outputs
            .get(self.output_name.as_str())
            .context("separator ONNX output is missing")?;
        let (shape, data) = output
            .try_extract_tensor::<f32>()
            .context("separator output is not a float tensor")?;
        anyhow::ensure!(
            **shape == [1_i64, 4, 2, DEMUCS_WINDOW_SAMPLES as i64],
            "separator output must have shape [1, 4, 2, {}]",
            DEMUCS_WINDOW_SAMPLES
        );
        let mut frames = Vec::with_capacity(DEMUCS_WINDOW_SAMPLES);
        for frame_index in 0..DEMUCS_WINDOW_SAMPLES {
            let mut frame = [[0.0_f32; 2]; 4];
            for (destination_stem, source_stem) in SOURCE_INDEX_FOR_CHAMPION_STEM.iter().enumerate()
            {
                let base = *source_stem * 2 * DEMUCS_WINDOW_SAMPLES;
                frame[destination_stem][0] = data[base + frame_index];
                frame[destination_stem][1] = data[base + DEMUCS_WINDOW_SAMPLES + frame_index];
            }
            frames.push(frame);
        }
        Ok(frames)
    }
}

pub(crate) fn validate_htdemucs_model(model_path: &Path) -> Result<()> {
    anyhow::ensure!(
        model_path.is_file()
            && model_path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("onnx")),
        "HT-Demucs model must be a local .onnx file"
    );
    let (session, _) = create_session(model_path, 1, false)?;
    validate_htdemucs_session(&session)
}

fn validate_htdemucs_session(session: &Session) -> Result<()> {
    anyhow::ensure!(
        session.inputs().len() == 1,
        "HT-Demucs must have exactly one input tensor"
    );
    anyhow::ensure!(
        session.outputs().len() == 1,
        "HT-Demucs must have exactly one output tensor"
    );
    let input = session.inputs().first().context("HT-Demucs has no input")?;
    let output = session
        .outputs()
        .first()
        .context("HT-Demucs has no output")?;
    let input_shape =
        float_tensor_shape(input.dtype()).context("HT-Demucs input must be float32")?;
    let output_shape =
        float_tensor_shape(output.dtype()).context("HT-Demucs output must be float32")?;
    anyhow::ensure!(
        input_shape.len() == 3
            && shape_axis_matches(input_shape[0], 1)
            && shape_axis_matches(input_shape[1], 2)
            && shape_axis_matches(input_shape[2], DEMUCS_WINDOW_SAMPLES as i64),
        "HT-Demucs input must be float32 [1, 2, {DEMUCS_WINDOW_SAMPLES}]"
    );
    anyhow::ensure!(
        output_shape.len() == 4
            && shape_axis_matches(output_shape[0], 1)
            && shape_axis_matches(output_shape[1], 4)
            && shape_axis_matches(output_shape[2], 2)
            && shape_axis_matches(output_shape[3], DEMUCS_WINDOW_SAMPLES as i64),
        "HT-Demucs output must be float32 [1, 4, 2, {DEMUCS_WINDOW_SAMPLES}]"
    );
    anyhow::ensure!(
        !input.name().is_empty() && !output.name().is_empty(),
        "HT-Demucs tensor names must not be empty"
    );
    Ok(())
}

fn float_tensor_shape(value_type: &ValueType) -> Result<Vec<i64>> {
    match value_type {
        ValueType::Tensor { ty, shape, .. } => {
            anyhow::ensure!(
                *ty == TensorElementType::Float32,
                "ONNX tensor must use float32 elements"
            );
            Ok(shape.iter().copied().collect())
        }
        _ => anyhow::bail!("ONNX value must be a tensor"),
    }
}

fn shape_axis_matches(actual: i64, expected: i64) -> bool {
    actual == expected || actual == -1
}

fn create_session(
    model_path: &Path,
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
            .and_then(|builder| builder.commit_from_file(model_path));
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
        .commit_from_file(model_path)
        .map_err(|error| {
            anyhow::anyhow!(
                "could not load local ONNX model {}: {error}",
                model_path.display()
            )
        })?;
    Ok((session, "CPU"))
}

fn fill_window(
    window: &mut [[f32; 2]],
    samples: &mut impl Iterator<Item = std::result::Result<i16, hound::Error>>,
    frames_read: &mut u64,
    total_frames: u64,
) -> Result<()> {
    for frame in window.iter_mut() {
        if *frames_read >= total_frames {
            *frame = [0.0; 2];
            continue;
        }
        let left = samples
            .next()
            .context("normalized WAV ended before its declared duration")??;
        let right = samples
            .next()
            .context("normalized WAV has an incomplete stereo frame")??;
        *frame = [i16_to_f32(left), i16_to_f32(right)];
        *frames_read += 1;
    }
    Ok(())
}

fn fill_window_tail(
    window: &mut [[f32; 2]],
    samples: &mut impl Iterator<Item = std::result::Result<i16, hound::Error>>,
    frames_read: &mut u64,
    total_frames: u64,
) -> Result<()> {
    let begin = window.len() - HOP_SAMPLES;
    for frame in &mut window[begin..] {
        if *frames_read >= total_frames {
            *frame = [0.0; 2];
            continue;
        }
        let left = samples
            .next()
            .context("normalized WAV ended before its declared duration")??;
        let right = samples
            .next()
            .context("normalized WAV has an incomplete stereo frame")??;
        *frame = [i16_to_f32(left), i16_to_f32(right)];
        *frames_read += 1;
    }
    Ok(())
}

fn i16_to_f32(sample: i16) -> f32 {
    (sample as f32 / i16::MAX as f32).clamp(-1.0, 1.0)
}

fn float_to_i16(sample: f32) -> i16 {
    let finite_sample = if sample.is_finite() { sample } else { 0.0 };
    (finite_sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn write_raw_frame(raw_files: &mut [BufWriter<File>], frame: &[[f32; 2]; 4]) -> Result<()> {
    for (index, file) in raw_files.iter_mut().enumerate() {
        file.write_all(&float_to_i16(frame[index][0]).to_le_bytes())?;
        file.write_all(&float_to_i16(frame[index][1]).to_le_bytes())?;
    }
    Ok(())
}

fn overlap_weight(index: usize) -> f32 {
    let phase = std::f32::consts::TAU * index as f32 / (DEMUCS_WINDOW_SAMPLES - 1) as f32;
    (0.5 - 0.5 * phase.cos()).max(OVERLAP_WEIGHT_FLOOR)
}

pub(crate) fn encode_recording_pcm16_to_flac(
    input_path: &Path,
    output_path: &Path,
    sample_rate: u32,
    frame_count: u64,
) -> Result<()> {
    anyhow::ensure!(
        (8_000..=192_000).contains(&sample_rate),
        "FLAC sample rate is unsupported"
    );
    anyhow::ensure!(frame_count > 0, "recording contains no PCM frames");
    let source_frames =
        usize::try_from(frame_count).context("recording is too long for FLAC encoding")?;
    let encoded_frames = source_frames.max(16);
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| anyhow::anyhow!("invalid FLAC encoder configuration: {error}"))?;
    let block_size = config.block_size;
    let max_block_size = block_size.min(encoded_frames);
    anyhow::ensure!(
        max_block_size >= 16 && max_block_size <= u16::MAX as usize,
        "FLAC block size is outside the supported range"
    );
    let mut stream_info = StreamInfo::new(sample_rate as usize, 2, 16)
        .map_err(|error| anyhow::anyhow!("invalid FLAC stream metadata: {error}"))?;
    stream_info
        .set_block_sizes(max_block_size, max_block_size)
        .map_err(|error| anyhow::anyhow!("invalid FLAC block-size metadata: {error}"))?;
    stream_info.set_total_samples(encoded_frames);
    let cancel = AtomicBool::new(false);
    encode_flac_streaming(
        input_path,
        output_path,
        source_frames,
        encoded_frames,
        sample_rate as usize,
        block_size,
        &config,
        &stream_info,
        &cancel,
    )
}

pub(super) fn encode_flac_streaming(
    input_path: &Path,
    output_path: &Path,
    source_frames: usize,
    encoded_frames: usize,
    sample_rate: usize,
    block_size: usize,
    config: &flacenc::error::Verified<flacenc::config::Encoder>,
    stream_info: &StreamInfo,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut output = File::create(output_path)
        .with_context(|| format!("cannot create FLAC output {}", output_path.display()))?;
    output.write_all(b"fLaC")?;
    // Last metadata block, STREAMINFO type 0, fixed body length of 34 bytes.
    output.write_all(&[0x80, 0, 0, 34])?;
    let mut stream_info_sink = ByteSink::new();
    stream_info
        .write(&mut stream_info_sink)
        .map_err(|error| anyhow::anyhow!("could not write FLAC STREAMINFO: {error:?}"))?;
    anyhow::ensure!(
        stream_info_sink.as_slice().len() == 34,
        "FLAC STREAMINFO has an invalid size"
    );
    output.write_all(stream_info_sink.as_slice())?;

    let mut source = FilePcm16Source::open(
        input_path,
        source_frames,
        encoded_frames,
        sample_rate,
        cancel,
    )?;
    let mut frame_buffer = FrameBuf::with_size(2, block_size)
        .map_err(|error| anyhow::anyhow!("could not allocate FLAC frame buffer: {error}"))?;
    let mut frame_number = 0_usize;
    let mut frames_written = 0_usize;
    loop {
        if cancel.load(Ordering::Acquire) {
            anyhow::bail!("import cancelled");
        }
        let frames_read = source
            .read_samples(block_size, &mut frame_buffer)
            .map_err(|error| anyhow::anyhow!("could not read PCM for FLAC: {error}"))?;
        if frames_read == 0 {
            break;
        }
        let frame =
            flacenc::encode_fixed_size_frame(config, &frame_buffer, frame_number, stream_info)
                .map_err(|error| {
                    anyhow::anyhow!("FLAC frame {frame_number} could not be encoded: {error}")
                })?;
        let mut frame_sink = ByteSink::new();
        frame.write(&mut frame_sink).map_err(|error| {
            anyhow::anyhow!("could not write FLAC frame {frame_number}: {error:?}")
        })?;
        output.write_all(frame_sink.as_slice())?;
        frame_number += 1;
        frames_written += frames_read;
    }
    anyhow::ensure!(
        frames_written == encoded_frames,
        "FLAC source supplied {frames_written} of {encoded_frames} samples"
    );
    output.flush()?;
    Ok(())
}

struct FilePcm16Source<'a> {
    file: File,
    source_frames: usize,
    encoded_frames: usize,
    sample_rate: usize,
    frames_read: usize,
    cancel: &'a AtomicBool,
}

impl<'a> FilePcm16Source<'a> {
    fn open(
        path: &Path,
        source_frames: usize,
        encoded_frames: usize,
        sample_rate: usize,
        cancel: &'a AtomicBool,
    ) -> Result<Self> {
        Ok(Self {
            file: File::open(path)
                .with_context(|| format!("cannot read FLAC input {}", path.display()))?,
            source_frames,
            encoded_frames,
            sample_rate,
            frames_read: 0,
            cancel,
        })
    }
}

impl Source for FilePcm16Source<'_> {
    fn channels(&self) -> usize {
        2
    }

    fn bits_per_sample(&self) -> usize {
        16
    }

    fn sample_rate(&self) -> usize {
        self.sample_rate
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> std::result::Result<usize, SourceError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(SourceError::from_io_error(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "import cancelled",
            )));
        }
        let frames = block_size.min(self.encoded_frames.saturating_sub(self.frames_read));
        if frames == 0 {
            return Ok(0);
        }
        let file_frames = frames.min(self.source_frames.saturating_sub(self.frames_read));
        let mut interleaved = Vec::<i32>::with_capacity(frames * 2);
        if file_frames > 0 {
            let mut bytes = vec![0_u8; file_frames * 2 * 2];
            self.file
                .read_exact(&mut bytes)
                .map_err(SourceError::from_io_error)?;
            interleaved.extend(
                bytes
                    .chunks_exact(2)
                    .map(|sample| i32::from(i16::from_le_bytes([sample[0], sample[1]]))),
            );
        }
        interleaved.resize(frames * 2, 0);
        dest.fill_interleaved(&interleaved)?;
        self.frames_read += frames;
        Ok(frames)
    }

    fn len_hint(&self) -> Option<usize> {
        Some(self.encoded_frames)
    }
}
