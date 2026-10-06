use crate::{
    audio::loudness::{LoudnessAnalysis, LoudnessAnalyzer},
    media_source,
};
use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavSpec, WavWriter};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, TrackType},
    io::MediaSourceStream,
    meta::{MetadataOptions, RawValue, StandardTag},
};

pub(crate) const MODEL_SAMPLE_RATE: u32 = 44_100;

#[derive(Clone, Copy, Debug)]
pub(crate) struct StereoFrame {
    pub left: f32,
    pub right: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct DecodeInfo {
    pub source_sample_rate: u32,
    pub source_channels: u32,
    pub source_bitrate_bps: Option<u64>,
    pub codec: String,
    pub tags_json: String,
    pub duration_ms: i64,
    pub frames: u64,
    pub loudness: LoudnessAnalysis,
    pub title: String,
    pub artist: String,
    pub tagged_bpm: Option<f64>,
}

/// Decode one source file to a bounded-size, temporary 44.1 kHz stereo PCM WAV.
/// The file is streamed packet-by-packet; the full recording is never retained in RAM.
pub(crate) fn decode_to_wav(
    source_path: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<DecodeInfo> {
    let encoded_bytes = std::fs::metadata(source_path)
        .with_context(|| format!("cannot inspect source audio {}", source_path.display()))?
        .len();
    let source = media_source::open_media_source(source_path)
        .with_context(|| format!("cannot open source audio {}", source_path.display()))?;
    let source = MediaSourceStream::new(source, Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &symphonia::core::probe::Hint::new(),
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .with_context(|| format!("unsupported audio container: {}", source_path.display()))?;
    let (track_id, codec_params) = {
        let track = format
            .first_track_known_codec(TrackType::Audio)
            .ok_or_else(|| {
                anyhow::anyhow!("no decodable audio track in {}", source_path.display())
            })?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("audio codec parameters are missing"))?;
        (track.id, params)
    };
    let mut decoder = crate::symphonia_support::codec_registry()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .with_context(|| format!("audio codec is unsupported: {}", source_path.display()))?;
    let codec = decoder.codec_info().short_name.to_owned();

    let spec = WavSpec {
        channels: 2,
        sample_rate: MODEL_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(destination, spec)
        .with_context(|| format!("cannot create temporary PCM file {}", destination.display()))?;
    let mut resampler: Option<LinearResampler> = None;
    let mut loudness: Option<LoudnessAnalyzer> = None;
    let mut input_rate = None;
    let mut input_channels = None;
    let mut output_frames = 0u64;
    let mut end_of_stream = false;

    while !end_of_stream {
        if cancel.load(Ordering::Acquire) {
            bail!("import cancelled");
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                end_of_stream = true;
                continue;
            }
            Err(SymphoniaError::ResetRequired) => {
                bail!("source stream changed its codec configuration mid-file")
            }
            Err(error) => return Err(error).context("audio demux failed"),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(error) => return Err(error).context("audio decode failed"),
        };
        let sample_rate = decoded.spec().rate();
        let channels = decoded.spec().channels().count();
        if sample_rate == 0 || channels == 0 {
            bail!("decoder reported an invalid sample rate or channel layout")
        }
        if input_rate.is_some_and(|rate| rate != sample_rate)
            || input_channels.is_some_and(|count| count != channels)
        {
            bail!("source audio format changed mid-file")
        }
        input_rate = Some(sample_rate);
        input_channels = Some(channels);

        let decoded_samples = decoded.samples_interleaved();
        let mut interleaved = vec![0.0f32; decoded_samples];
        decoded.copy_to_slice_interleaved(&mut interleaved);
        let resampler =
            resampler.get_or_insert_with(|| LinearResampler::new(sample_rate, MODEL_SAMPLE_RATE));
        let loudness = loudness.get_or_insert_with(|| LoudnessAnalyzer::new(sample_rate));
        for channel_frame in interleaved.chunks_exact(channels) {
            // For multichannel sources the first two channels are kept. Mono is duplicated.
            // This matches the playback decoder's existing stereo policy.
            let left = finite_sample(channel_frame[0]);
            let right = if channels > 1 {
                finite_sample(channel_frame[1])
            } else {
                left
            };
            // Measure the original-rate decoded stereo stream before resampling and PCM16
            // conversion, preserving source inter-sample peaks and the proper filter coefficients.
            loudness.push_stereo(left, if channels > 1 { right } else { 0.0 });
            resampler.push(StereoFrame { left, right });
            while let Some(frame) = resampler.next_frame(false) {
                if output_frames % 65_536 == 0 && cancel.load(Ordering::Acquire) {
                    bail!("import cancelled");
                }
                let frame = StereoFrame {
                    left: finite_sample(frame.left).clamp(-1.0, 1.0),
                    right: finite_sample(frame.right).clamp(-1.0, 1.0),
                };
                write_pcm16(&mut writer, frame)?;
                output_frames += 1;
            }
        }
    }

    if let Some(resampler) = &mut resampler {
        while let Some(frame) = resampler.next_frame(true) {
            if output_frames % 65_536 == 0 && cancel.load(Ordering::Acquire) {
                bail!("import cancelled");
            }
            let frame = StereoFrame {
                left: finite_sample(frame.left).clamp(-1.0, 1.0),
                right: finite_sample(frame.right).clamp(-1.0, 1.0),
            };
            write_pcm16(&mut writer, frame)?;
            output_frames += 1;
        }
    }
    let loudness = loudness
        .ok_or_else(|| anyhow::anyhow!("source contains no decodable audio frames"))?
        .finish();
    writer.finalize().with_context(|| {
        format!(
            "could not finalize temporary PCM file {}",
            destination.display()
        )
    })?;

    if output_frames == 0 {
        bail!("source contains no decodable audio frames")
    }
    let source_sample_rate = input_rate.unwrap_or(MODEL_SAMPLE_RATE);
    let source_channels = input_channels.unwrap_or(2) as u32;
    let fallback_title = source_path
        .file_stem()
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Untitled track".to_owned());
    let mut title = None;
    let mut artist = None;
    let mut tagged_bpm = None;
    let mut tags = BTreeMap::<String, Vec<String>>::new();
    let mut tag_bytes = 0usize;
    const MAX_TAG_BYTES: usize = 1_048_576;
    {
        let mut metadata = format.metadata();
        loop {
            if let Some(revision) = metadata.current() {
                for tag in &revision.media.tags {
                    match &tag.std {
                        Some(StandardTag::TrackTitle(value)) => title = Some(value.to_string()),
                        Some(StandardTag::Artist(value)) => artist = Some(value.to_string()),
                        Some(StandardTag::Bpm(value)) => tagged_bpm = Some(*value as f64),
                        _ => {}
                    }
                    if tags.len() >= 256 || matches!(&tag.raw.value, RawValue::Binary(_)) {
                        continue;
                    }
                    let key = tag.raw.key.trim();
                    if key.is_empty() || key.len() > 256 {
                        continue;
                    }
                    if tag_bytes >= MAX_TAG_BYTES {
                        continue;
                    }
                    let value = tag.raw.value.to_string();
                    let mut value = value.chars().take(4_096).collect::<String>();
                    let remaining = MAX_TAG_BYTES - tag_bytes;
                    if value.len() > remaining {
                        let mut boundary = remaining;
                        while !value.is_char_boundary(boundary) {
                            boundary -= 1;
                        }
                        value.truncate(boundary);
                    }
                    tag_bytes += value.len();
                    let values = tags.entry(key.to_owned()).or_default();
                    if values.len() < 32 {
                        values.push(value);
                    }
                }
            }
            if metadata.is_latest() {
                break;
            }
            metadata.pop();
        }
    }
    let tags_json =
        serde_json::to_string(&tags).context("could not serialize source audio tags")?;
    let duration_ms = (output_frames.saturating_mul(1_000) / u64::from(MODEL_SAMPLE_RATE)) as i64;
    let source_bitrate_bps = if duration_ms > 0 {
        let bitrate = u128::from(encoded_bytes)
            .saturating_mul(8)
            .saturating_mul(u128::from(MODEL_SAMPLE_RATE))
            / u128::from(output_frames.max(1));
        Some(bitrate.min(100_000_000) as u64)
    } else {
        None
    };
    Ok(DecodeInfo {
        source_sample_rate,
        source_channels,
        source_bitrate_bps,
        codec,
        tags_json,
        duration_ms,
        frames: output_frames,
        loudness,
        title: title
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(fallback_title),
        artist: artist.unwrap_or_default(),
        tagged_bpm: tagged_bpm.filter(|bpm| (30.0..=300.0).contains(bpm)),
    })
}

fn write_pcm16(writer: &mut WavWriter<std::io::BufWriter<File>>, frame: StereoFrame) -> Result<()> {
    writer.write_sample(
        (finite_sample(frame.left).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16,
    )?;
    writer.write_sample(
        (finite_sample(frame.right).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16,
    )?;
    Ok(())
}

fn finite_sample(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

struct LinearResampler {
    input_rate: u32,
    output_step: f64,
    phase: f64,
    input: VecDeque<StereoFrame>,
}

impl LinearResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate,
            output_step: f64::from(input_rate) / f64::from(output_rate.max(1)),
            phase: 0.0,
            input: VecDeque::with_capacity(4096),
        }
    }

    fn push(&mut self, frame: StereoFrame) {
        self.input.push_back(frame);
    }

    fn next_frame(&mut self, flush: bool) -> Option<StereoFrame> {
        while self.phase >= 1.0 && !self.input.is_empty() {
            self.input.pop_front();
            self.phase -= 1.0;
        }
        if self.input.len() >= 2 {
            let first = *self.input.front()?;
            let second = *self.input.get(1)?;
            let fraction = self.phase as f32;
            self.phase += self.output_step;
            Some(StereoFrame {
                left: first.left + (second.left - first.left) * fraction,
                right: first.right + (second.right - first.right) * fraction,
            })
        } else if flush && self.input.len() == 1 {
            self.phase = 0.0;
            self.input.pop_front()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LinearResampler, StereoFrame};

    #[test]
    fn linear_resampler_preserves_equal_rate_and_interpolates_up() {
        let mut resampler = LinearResampler::new(24_000, 48_000);
        for value in [0.0, 1.0, 2.0] {
            resampler.push(StereoFrame {
                left: value,
                right: value,
            });
        }
        let output: Vec<f32> = std::iter::from_fn(|| resampler.next_frame(true))
            .map(|frame| frame.left)
            .collect();
        assert_eq!(output, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
    }
}
