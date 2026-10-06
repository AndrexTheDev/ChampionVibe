use super::{
    wsola::{SharedSchedule, WsolaProcessor},
    EngineStatus, PitchFrame, StereoFrame,
};
use crate::media_source;
use rtrb::{Producer, PushError};
use std::{
    collections::VecDeque,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use symphonia::core::{
    codecs::audio::{AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO},
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

pub(super) fn run_worker(
    path: PathBuf,
    output_rate: u32,
    start_offset_frames: u64,
    playback_rate: f32,
    stem_index: usize,
    schedule: Arc<SharedSchedule>,
    prebuffer_frames: usize,
    mut producer: Producer<PitchFrame>,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    startup: Sender<Result<(), String>>,
    status: Arc<EngineStatus>,
) {
    let mut ready_sent = false;
    let mut produced_frames = 0usize;
    let decoder = match StemDecoder::open(&path, output_rate) {
        Ok(decoder) => decoder,
        Err(error) => {
            if stem_index == 1 {
                schedule.finish();
            }
            report_worker_error(&path, error, &status, &finished, &startup, &mut ready_sent);
            return;
        }
    };
    let tempo_changed = (playback_rate - 1.0).abs() > 0.0005;
    let mut source =
        SharedStemSource::new(decoder, start_offset_frames, usize::from(tempo_changed) + 1);
    let mut stretcher = if tempo_changed {
        match WsolaProcessor::new(
            output_rate,
            playback_rate,
            stem_index == 1,
            Arc::clone(&schedule),
        ) {
            Ok(processor) => Some(processor),
            Err(error) => {
                if stem_index == 1 {
                    schedule.finish();
                }
                report_worker_error(&path, error, &status, &finished, &startup, &mut ready_sent);
                return;
            }
        }
    } else {
        None
    };
    let mut varispeed = tempo_changed.then(|| VarispeedProcessor::new(playback_rate));

    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }

        let decoded = if tempo_changed {
            let wsola_frame = match stretcher.as_mut() {
                Some(stretcher) => stretcher.next_frame(&mut || source.next_frame_for(0), &cancel),
                None => unreachable!("tempo-changing stems have a WSOLA processor"),
            };
            let wsola_frame = match wsola_frame {
                Ok(frame) => frame,
                Err(error) => {
                    if stem_index == 1 {
                        schedule.finish();
                    }
                    report_worker_error(
                        &path,
                        error,
                        &status,
                        &finished,
                        &startup,
                        &mut ready_sent,
                    );
                    return;
                }
            };
            let varispeed_frame = match varispeed.as_mut() {
                Some(varispeed) => varispeed.next_frame(&mut || source.next_frame_for(1)),
                None => unreachable!("tempo-changing stems have a varispeed processor"),
            };
            match varispeed_frame {
                Ok(varispeed_frame) => match (wsola_frame, varispeed_frame) {
                    (Some(wsola), Some(varispeed)) => Ok(Some(PitchFrame { wsola, varispeed })),
                    (Some(wsola), None) => Ok(Some(PitchFrame {
                        wsola,
                        varispeed: StereoFrame::default(),
                    })),
                    (None, Some(varispeed)) => Ok(Some(PitchFrame {
                        wsola: StereoFrame::default(),
                        varispeed,
                    })),
                    (None, None) => Ok(None),
                },
                Err(error) => {
                    if stem_index == 1 {
                        schedule.finish();
                    }
                    report_worker_error(
                        &path,
                        error,
                        &status,
                        &finished,
                        &startup,
                        &mut ready_sent,
                    );
                    return;
                }
            }
        } else {
            source.next_frame_for(0).map(|frame| {
                frame.map(|frame| PitchFrame {
                    wsola: frame,
                    varispeed: frame,
                })
            })
        };

        match decoded {
            Ok(Some(frame)) => {
                let mut pending = frame;
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    match producer.push(pending) {
                        Ok(()) => {
                            produced_frames += 1;
                            break;
                        }
                        Err(PushError::Full(returned)) => {
                            pending = returned;
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }

                if !ready_sent && produced_frames >= prebuffer_frames {
                    let _ = startup.send(Ok(()));
                    ready_sent = true;
                }
            }
            Ok(None) => {
                if stem_index == 1 {
                    schedule.finish();
                }
                finished.store(true, Ordering::Release);
                if !ready_sent {
                    if produced_frames == 0 {
                        let message = if start_offset_frames > 0 {
                            "requested cue is beyond the end of this stem"
                        } else {
                            "stem contains no decodable audio frames"
                        };
                        report_worker_error(
                            &path,
                            message.to_owned(),
                            &status,
                            &finished,
                            &startup,
                            &mut ready_sent,
                        );
                    } else {
                        let _ = startup.send(Ok(()));
                        ready_sent = true;
                    }
                }
                return;
            }
            Err(error) => {
                if stem_index == 1 {
                    schedule.finish();
                }
                report_worker_error(&path, error, &status, &finished, &startup, &mut ready_sent);
                return;
            }
        }
    }
}

/// Simple streaming varispeed used when Pitch-Lock is disabled. It deliberately changes pitch
/// along with tempo and keeps the same rate for every aligned stem.
struct VarispeedProcessor {
    speed: f64,
    source_position: f64,
    input: VecDeque<StereoFrame>,
    input_base: u64,
    source_finished: bool,
}

impl VarispeedProcessor {
    fn new(speed: f32) -> Self {
        Self {
            speed: f64::from(speed.clamp(0.8, 1.2)),
            source_position: 0.0,
            input: VecDeque::with_capacity(4),
            input_base: 0,
            source_finished: false,
        }
    }

    fn next_frame<F>(&mut self, next_input: &mut F) -> Result<Option<StereoFrame>, String>
    where
        F: FnMut() -> Result<Option<StereoFrame>, String>,
    {
        let first_index = self.source_position.floor().max(0.0) as u64;
        let second_index = first_index.saturating_add(1);
        while self.input_base.saturating_add(self.input.len() as u64) <= second_index
            && !self.source_finished
        {
            match next_input()? {
                Some(frame) => self.input.push_back(frame),
                None => self.source_finished = true,
            }
        }
        let input_end = self.input_base.saturating_add(self.input.len() as u64);
        if first_index >= input_end {
            return Ok(None);
        }
        let first_offset = usize::try_from(first_index.saturating_sub(self.input_base))
            .map_err(|_| "varispeed frame index overflow".to_owned())?;
        let Some(first) = self.input.get(first_offset).copied() else {
            return Ok(None);
        };
        let second_offset = usize::try_from(second_index.saturating_sub(self.input_base))
            .map_err(|_| "varispeed frame index overflow".to_owned())?;
        let second = self.input.get(second_offset).copied().unwrap_or(first);
        let fraction = (self.source_position - first_index as f64).clamp(0.0, 1.0) as f32;
        let output = StereoFrame {
            left: first.left + (second.left - first.left) * fraction,
            right: first.right + (second.right - first.right) * fraction,
        };
        self.source_position += self.speed;
        while self.input_base < first_index {
            self.input.pop_front();
            self.input_base = self.input_base.saturating_add(1);
        }
        Ok(Some(output))
    }
}

fn report_worker_error(
    path: &Path,
    error: String,
    status: &EngineStatus,
    finished: &AtomicBool,
    startup: &Sender<Result<(), String>>,
    ready_sent: &mut bool,
) {
    let message = format!("could not stream stem {}: {error}", path.display());
    if let Ok(mut last_error) = status.last_error.lock() {
        *last_error = Some(message.clone());
    }
    status.failed.store(true, Ordering::Release);
    finished.store(true, Ordering::Release);
    if !*ready_sent {
        let _ = startup.send(Err(message));
        *ready_sent = true;
    }
}

/// One decoded stem stream shared by the WSOLA and varispeed processors. The cache retains
/// only audio not yet consumed by both processors, so pitch-lock transitions do not require a
/// second decoder or a track reload.
struct SharedStemSource {
    decoder: StemDecoder,
    skip_remaining: u64,
    frames: VecDeque<StereoFrame>,
    base_index: u64,
    next_index: u64,
    cursors: [u64; 2],
    branch_count: usize,
    finished: bool,
}

impl SharedStemSource {
    fn new(decoder: StemDecoder, skip_remaining: u64, branch_count: usize) -> Self {
        Self {
            decoder,
            skip_remaining,
            frames: VecDeque::with_capacity(4_096),
            base_index: 0,
            next_index: 0,
            cursors: [0, 0],
            branch_count: branch_count.clamp(1, 2),
            finished: false,
        }
    }

    fn next_frame_for(&mut self, branch: usize) -> Result<Option<StereoFrame>, String> {
        if branch >= self.branch_count {
            return Err("invalid internal stem-processing branch".to_owned());
        }
        let cursor = self.cursors[branch];
        while cursor >= self.next_index && !self.finished {
            while self.skip_remaining > 0 {
                match self.decoder.next_frame()? {
                    Some(_) => self.skip_remaining -= 1,
                    None => {
                        self.finished = true;
                        return Ok(None);
                    }
                }
            }
            match self.decoder.next_frame()? {
                Some(frame) => {
                    self.frames.push_back(frame);
                    self.next_index = self.next_index.saturating_add(1);
                }
                None => self.finished = true,
            }
        }

        if cursor >= self.next_index {
            return Ok(None);
        }
        let offset = cursor.saturating_sub(self.base_index) as usize;
        let frame =
            self.frames.get(offset).copied().ok_or_else(|| {
                "internal stem branch requested a released source frame".to_owned()
            })?;
        self.cursors[branch] = cursor.saturating_add(1);

        let consumed_by_all = self.cursors[..self.branch_count]
            .iter()
            .copied()
            .min()
            .unwrap_or(self.next_index);
        while self.base_index < consumed_by_all {
            self.frames.pop_front();
            self.base_index = self.base_index.saturating_add(1);
        }
        Ok(Some(frame))
    }
}

struct StemDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    output_rate: u32,
    resampler: Option<LinearResampler>,
    end_of_stream: bool,
}

impl StemDecoder {
    fn open(path: &Path, output_rate: u32) -> Result<Self, String> {
        let source = media_source::open_media_source(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let source = MediaSourceStream::new(source, Default::default());
        let mut format = symphonia::default::get_probe()
            .probe(
                &symphonia::core::probe::Hint::new(),
                source,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|error| {
                format!("{}: unsupported audio container ({error})", path.display())
            })?;
        let (track_id, codec_parameters) = format
            .tracks()
            .iter()
            .find_map(|track| {
                let codec_parameters = track.codec_params.as_ref()?.audio()?;
                (codec_parameters.codec != CODEC_ID_NULL_AUDIO)
                    .then(|| (track.id, codec_parameters.clone()))
            })
            .ok_or_else(|| format!("{}: no decodable audio track", path.display()))?;
        let decoder = crate::symphonia_support::codec_registry()
            .make_audio_decoder(&codec_parameters, &AudioDecoderOptions::default())
            .map_err(|error| format!("{}: codec is not supported ({error})", path.display()))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            output_rate,
            resampler: None,
            end_of_stream: false,
        })
    }

    fn next_frame(&mut self) -> Result<Option<StereoFrame>, String> {
        loop {
            if let Some(resampler) = &mut self.resampler {
                if let Some(frame) = resampler.next_frame(self.end_of_stream) {
                    return Ok(Some(frame));
                }
            }
            if self.end_of_stream {
                return Ok(None);
            }

            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.end_of_stream = true;
                    continue;
                }
                Err(SymphoniaError::ResetRequired) => {
                    return Err("stream configuration changed mid-file".to_owned());
                }
                Err(SymphoniaError::IoError(error))
                    if error.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    self.end_of_stream = true;
                    continue;
                }
                Err(error) => return Err(format!("demux error: {error}")),
            };
            if packet.track_id != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(error) => return Err(format!("decode error: {error}")),
            };
            let sample_rate = decoded.spec().rate();
            let channel_count = decoded.spec().channels().count();
            if sample_rate == 0 || channel_count == 0 {
                return Err("decoder reported an invalid channel layout or sample rate".to_owned());
            }

            let mut samples = vec![0.0f32; decoded.samples_interleaved()];
            decoded.copy_to_slice_interleaved(&mut samples[..]);

            let resampler = self
                .resampler
                .get_or_insert_with(|| LinearResampler::new(sample_rate, self.output_rate));
            if resampler.input_rate != sample_rate {
                return Err("stem sample rate changed while decoding".to_owned());
            }
            for channels in samples.chunks_exact(channel_count) {
                let left = channels[0];
                let right = if channel_count > 1 { channels[1] } else { left };
                resampler.push(StereoFrame { left, right });
            }
        }
    }
}

/// CVW1 packet: 16-byte header (magic, peak count, beat count, BPM), followed by
/// u16 peak magnitudes and u32 beat positions in milliseconds, all little-endian.
pub(super) fn waveform_packet(
    path: &Path,
    duration_ms: Option<i64>,
    bpm: Option<f64>,
    beat_times_seconds: &[f64],
    peak_count: usize,
) -> Result<Vec<u8>, String> {
    const PEAK_SAMPLE_RATE: u32 = 11_025;
    let peak_count = peak_count.clamp(128, 2048);
    let mut peaks = vec![0.0_f32; peak_count];

    if let Some(duration_ms) = duration_ms.filter(|duration| *duration > 0) {
        let duration_ms = duration_ms.min(86_400_000) as u64;
        let expected_frames =
            (duration_ms.saturating_mul(u64::from(PEAK_SAMPLE_RATE)) / 1_000).max(1);
        let mut decoder = StemDecoder::open(path, PEAK_SAMPLE_RATE)?;
        let mut frame_index = 0_u64;
        while frame_index < expected_frames {
            let Some(frame) = decoder.next_frame()? else {
                break;
            };
            let bucket = (frame_index.saturating_mul(peak_count as u64) / expected_frames)
                .min((peak_count - 1) as u64) as usize;
            let magnitude = frame.left.abs().max(frame.right.abs());
            if magnitude.is_finite() {
                peaks[bucket] = peaks[bucket].max(magnitude.clamp(0.0, 1.0));
            }
            frame_index += 1;
        }
    }

    let mut peak_blob = Vec::with_capacity(peak_count * 2);
    for peak in peaks {
        let value = (peak.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16;
        peak_blob.extend_from_slice(&value.to_le_bytes());
    }
    waveform_packet_from_peak_blob(&peak_blob, bpm, beat_times_seconds, peak_count)
}

pub(super) fn waveform_packet_from_peak_blob(
    peaks_blob: &[u8],
    bpm: Option<f64>,
    beat_times_seconds: &[f64],
    peak_count: usize,
) -> Result<Vec<u8>, String> {
    const MAX_BEATS: usize = 200_000;
    let peak_count = peak_count.clamp(128, 2048);
    if peaks_blob.len() != peak_count * 2 {
        return Err("waveform peak BLOB has an invalid length".to_owned());
    }
    let beats: Vec<u32> = beat_times_seconds
        .iter()
        .filter(|time| time.is_finite() && **time >= 0.0 && **time <= 4_294_967.0)
        .take(MAX_BEATS)
        .map(|time| (*time * 1_000.0).round() as u32)
        .collect();
    let bpm = bpm
        .filter(|value| value.is_finite() && (0.0..=1_000.0).contains(value))
        .unwrap_or(0.0) as f32;

    let mut packet = Vec::with_capacity(16 + peaks_blob.len() + beats.len() * 4);
    packet.extend_from_slice(b"CVW1");
    packet.extend_from_slice(&(peak_count as u32).to_le_bytes());
    packet.extend_from_slice(&(beats.len() as u32).to_le_bytes());
    packet.extend_from_slice(&bpm.to_le_bytes());
    packet.extend_from_slice(peaks_blob);
    for beat_ms in beats {
        packet.extend_from_slice(&beat_ms.to_le_bytes());
    }
    Ok(packet)
}

struct LinearResampler {
    input_rate: u32,
    input_step: f64,
    phase: f64,
    input: VecDeque<StereoFrame>,
}

impl LinearResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate,
            input_step: input_rate as f64 / output_rate.max(1) as f64,
            phase: 0.0,
            input: VecDeque::with_capacity(4096),
        }
    }

    fn push(&mut self, frame: StereoFrame) {
        self.input.push_back(frame);
    }

    fn next_frame(&mut self, flush: bool) -> Option<StereoFrame> {
        // For downsampling, the fractional source position can skip across a
        // packet boundary. Preserve that skip until the next decoded packet.
        while self.phase >= 1.0 && !self.input.is_empty() {
            self.input.pop_front();
            self.phase -= 1.0;
        }

        if self.input.len() >= 2 {
            let first = *self.input.front()?;
            let second = *self.input.get(1)?;
            let fraction = self.phase as f32;
            let output = StereoFrame {
                left: first.left + (second.left - first.left) * fraction,
                right: first.right + (second.right - first.right) * fraction,
            };
            self.phase += self.input_step;
            Some(output)
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
    use super::{waveform_packet_from_peak_blob, LinearResampler, StereoFrame, VarispeedProcessor};

    #[test]
    fn equal_rate_resampling_preserves_frames_and_flushes_tail() {
        let mut resampler = LinearResampler::new(48_000, 48_000);
        for value in [0.0, 1.0, 2.0] {
            resampler.push(StereoFrame {
                left: value,
                right: value,
            });
        }

        assert_eq!(resampler.next_frame(false).unwrap().left, 0.0);
        assert_eq!(resampler.next_frame(false).unwrap().left, 1.0);
        assert!(resampler.next_frame(false).is_none());
        assert_eq!(resampler.next_frame(true).unwrap().left, 2.0);
        assert!(resampler.next_frame(true).is_none());
    }

    #[test]
    fn upsampling_interpolates_between_source_frames() {
        let mut resampler = LinearResampler::new(24_000, 48_000);
        for value in [0.0, 1.0, 2.0] {
            resampler.push(StereoFrame {
                left: value,
                right: value,
            });
        }

        let output: Vec<_> = std::iter::from_fn(|| resampler.next_frame(true))
            .map(|frame| frame.left)
            .collect();
        assert_eq!(output, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
    }

    #[test]
    fn disabled_pitch_lock_uses_bounded_streaming_varispeed() {
        let mut source = std::collections::VecDeque::from(vec![
            StereoFrame {
                left: 0.0,
                right: 0.0,
            },
            StereoFrame {
                left: 1.0,
                right: 1.0,
            },
            StereoFrame {
                left: 2.0,
                right: 2.0,
            },
            StereoFrame {
                left: 3.0,
                right: 3.0,
            },
        ]);
        let mut next = || Ok(source.pop_front());
        let mut processor = VarispeedProcessor::new(1.5);
        let output = std::iter::from_fn(|| processor.next_frame(&mut next).unwrap())
            .map(|frame| frame.left)
            .collect::<Vec<_>>();
        assert_eq!(output, vec![0.0, 1.5, 3.0]);
    }

    #[test]
    fn cached_peak_blob_is_packed_with_the_current_binary_beat_grid() {
        let peaks = [0x34, 0x12].repeat(128);
        let packet = waveform_packet_from_peak_blob(&peaks, Some(120.0), &[0.0, 0.5], 128).unwrap();
        assert_eq!(&packet[0..4], b"CVW1");
        assert_eq!(u32::from_le_bytes(packet[4..8].try_into().unwrap()), 128);
        assert_eq!(u32::from_le_bytes(packet[8..12].try_into().unwrap()), 2);
        assert_eq!(&packet[16..16 + peaks.len()], peaks.as_slice());
        assert_eq!(
            u32::from_le_bytes(
                packet[16 + peaks.len()..20 + peaks.len()]
                    .try_into()
                    .unwrap()
            ),
            0
        );
        assert_eq!(
            u32::from_le_bytes(
                packet[20 + peaks.len()..24 + peaks.len()]
                    .try_into()
                    .unwrap()
            ),
            500
        );
        assert!(waveform_packet_from_peak_blob(&[0, 1], Some(120.0), &[], 128).is_err());
    }
}
