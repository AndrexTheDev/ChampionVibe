use super::StereoFrame;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};
use serde::Serialize;

const CAPTURE_RING_FRAMES: usize = 65_536;
const ALIGNMENT_LOOKAHEAD_FRAMES: u64 = 8_192;
const MAX_PENDING_FRAMES: u64 = 131_072;
const STOP_DRAIN_GRACE: Duration = Duration::from_millis(12);
const WORKER_POLL: Duration = Duration::from_millis(2);
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(180);
const REASON_NONE: u8 = 0;
const REASON_USER_STOP: u8 = 1;
const REASON_LICENSE_EXPIRED: u8 = 2;
const REASON_RING_OVERFLOW: u8 = 3;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CapturedFrame {
    pub timestamp_ns: u64,
    pub left: f32,
    pub right: f32,
}

pub(crate) struct CaptureClock {
    origin: Instant,
}

impl CaptureClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub(crate) fn now_ns(&self) -> u64 {
        self.origin.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
}

pub(crate) struct CaptureFlags {
    active: AtomicBool,
    start_ns: AtomicU64,
    deadline_ns: AtomicU64,
    stop_reason: AtomicU8,
    dropped_frames: AtomicU64,
}

impl CaptureFlags {
    fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            start_ns: AtomicU64::new(0),
            deadline_ns: AtomicU64::new(u64::MAX),
            stop_reason: AtomicU8::new(REASON_NONE),
            dropped_frames: AtomicU64::new(0),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn should_capture(&self, timestamp_ns: u64) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if timestamp_ns < self.start_ns.load(Ordering::Relaxed) {
            return false;
        }
        if timestamp_ns >= self.deadline_ns.load(Ordering::Relaxed) {
            self.stop_reason
                .compare_exchange(
                    REASON_NONE,
                    REASON_LICENSE_EXPIRED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .ok();
            self.active.store(false, Ordering::Release);
            return false;
        }
        true
    }

    pub(crate) fn record_drop(&self) {
        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
        self.stop_reason
            .compare_exchange(
                REASON_NONE,
                REASON_RING_OVERFLOW,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok();
        self.active.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordingFormat {
    WavPcm16,
    WavPcm24,
    Flac,
    Mp3_320,
    OggVorbis,
    M4aAac,
}

impl RecordingFormat {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("wavPcm16") {
            "wavPcm16" | "wav" | "wav16" => Ok(Self::WavPcm16),
            "wavPcm24" | "wav24" => Ok(Self::WavPcm24),
            "flac" => Ok(Self::Flac),
            "mp3" | "mp3_320" => Ok(Self::Mp3_320),
            "oggVorbis" | "ogg" | "vorbis" => Ok(Self::OggVorbis),
            "m4aAac" | "m4a" | "aac" => Ok(Self::M4aAac),
            _ => Err(
                "Unknown recording format. Choose WAV, FLAC, MP3, OGG/Vorbis, or M4A/AAC.".into(),
            ),
        }
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::WavPcm16 | Self::WavPcm24 => "wav",
            Self::Flac => "flac",
            Self::Mp3_320 => "mp3",
            Self::OggVorbis => "ogg",
            Self::M4aAac => "m4a",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::WavPcm16 => "wavPcm16",
            Self::WavPcm24 => "wavPcm24",
            Self::Flac => "flac",
            Self::Mp3_320 => "mp3",
            Self::OggVorbis => "oggVorbis",
            Self::M4aAac => "m4aAac",
        }
    }

    fn bits_per_sample(self) -> u16 {
        match self {
            Self::WavPcm24 => 24,
            Self::WavPcm16 | Self::Flac | Self::Mp3_320 | Self::OggVorbis | Self::M4aAac => 16,
        }
    }

    fn output_sample_rate(self, input_sample_rate: u32) -> u32 {
        match self {
            Self::Mp3_320 => 44_100,
            Self::OggVorbis | Self::M4aAac => 48_000,
            Self::WavPcm16 | Self::WavPcm24 | Self::Flac => input_sample_rate,
        }
    }

    pub(crate) fn nominal_bitrate_bps(self, sample_rate: u32) -> u32 {
        match self {
            Self::Mp3_320 => 320_000,
            Self::OggVorbis => 0, // variable bitrate; no fixed-rate promise
            Self::M4aAac => 160_000,
            Self::WavPcm16 => sample_rate.saturating_mul(2 * 16),
            Self::WavPcm24 => sample_rate.saturating_mul(2 * 24),
            Self::Flac => 0, // lossless bitrate is content-dependent
        }
    }

    fn hound_spec(self, sample_rate: u32) -> hound::WavSpec {
        hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: self.bits_per_sample(),
            sample_format: hound::SampleFormat::Int,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingSnapshot {
    pub active: bool,
    pub status_code: String,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub path: Option<String>,
    pub frames_written: u64,
    pub dropped_frames: u64,
    pub error: Option<String>,
}

impl Default for RecordingSnapshot {
    fn default() -> Self {
        Self {
            active: false,
            status_code: "recording.ready".into(),
            format: None,
            sample_rate: None,
            bit_depth: None,
            path: None,
            frames_written: 0,
            dropped_frames: 0,
            error: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecordingFile {
    pub name: String,
    pub path: String,
    pub format: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
    pub frames: u64,
    pub size_bytes: u64,
}

pub(crate) struct RecordingHub {
    pub(crate) clock: Arc<CaptureClock>,
    pub(crate) flags: Arc<CaptureFlags>,
    sender: SyncSender<WorkerCommand>,
    snapshot: Arc<Mutex<RecordingSnapshot>>,
    stream_rate: AtomicU32,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RecordingHub {
    pub(crate) fn new() -> Result<Arc<Self>, String> {
        let clock = Arc::new(CaptureClock::new());
        let flags = Arc::new(CaptureFlags::new());
        let snapshot = Arc::new(Mutex::new(RecordingSnapshot::default()));
        let (sender, receiver) = mpsc::sync_channel(32);
        let worker_flags = Arc::clone(&flags);
        let worker_clock = Arc::clone(&clock);
        let worker_snapshot = Arc::clone(&snapshot);
        let worker = thread::Builder::new()
            .name("championvibe-master-recording".into())
            .spawn(move || {
                lower_worker_priority();
                worker_loop(receiver, worker_flags, worker_clock, worker_snapshot);
            })
            .map_err(|error| format!("Could not start the recording worker: {error}"))?;
        Ok(Arc::new(Self {
            clock,
            flags,
            sender,
            snapshot,
            stream_rate: AtomicU32::new(0),
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub(crate) fn new_capture_ring(&self) -> (Producer<CapturedFrame>, Consumer<CapturedFrame>) {
        RingBuffer::new(CAPTURE_RING_FRAMES)
    }

    pub(crate) fn attach(
        &self,
        consumer: Consumer<CapturedFrame>,
        sample_rate: u32,
    ) -> Result<(), String> {
        if self.stream_rate.load(Ordering::Acquire) != 0 {
            return Err("the post-master recording tap is already attached".into());
        }
        self.sender
            .send(WorkerCommand::Attach { consumer })
            .map_err(|_| "The recording worker is not available.".to_string())?;
        self.stream_rate.store(sample_rate, Ordering::Release);
        Ok(())
    }

    pub(crate) fn detach(&self) {
        self.stream_rate.store(0, Ordering::Release);
        let _ = self.sender.send(WorkerCommand::Detach);
    }

    pub(crate) fn sample_rate(&self) -> Option<u32> {
        let rate = self.stream_rate.load(Ordering::Acquire);
        (rate > 0).then_some(rate)
    }

    pub(crate) fn start(
        &self,
        path: PathBuf,
        format: RecordingFormat,
        sample_rate: u32,
        license_deadline_ns: u64,
    ) -> Result<RecordingSnapshot, String> {
        if sample_rate < 8_000 || sample_rate > 192_000 {
            return Err("The active audio device has an unsupported recording sample rate.".into());
        }
        if self.flags.active.load(Ordering::Acquire)
            || self
                .snapshot
                .lock()
                .map_err(|_| "Recording status is unavailable.")?
                .active
        {
            return Err("A master recording is already running.".into());
        }
        let start_ns = self.clock.now_ns();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.sender
            .send(WorkerCommand::Start {
                path,
                format,
                sample_rate,
                start_ns,
                license_deadline_ns,
                reply: reply_tx,
            })
            .map_err(|_| "The recording worker is not available.".to_string())?;
        reply_rx
            .recv_timeout(START_TIMEOUT)
            .map_err(|_| "The recording worker did not start in time.".to_string())??;
        if self.clock.now_ns() >= license_deadline_ns {
            self.flags
                .stop_reason
                .store(REASON_LICENSE_EXPIRED, Ordering::Release);
            self.flags.active.store(false, Ordering::Release);
            return Err("Pro license expired; recording is unavailable.".into());
        }
        self.flags.start_ns.store(start_ns, Ordering::Relaxed);
        self.flags
            .deadline_ns
            .store(license_deadline_ns, Ordering::Relaxed);
        self.flags.dropped_frames.store(0, Ordering::Relaxed);
        self.flags.stop_reason.store(REASON_NONE, Ordering::Relaxed);
        self.flags.active.store(true, Ordering::Release);
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| "Recording status is unavailable.".to_string())
    }

    pub(crate) fn stop(&self) -> Result<RecordingFile, String> {
        if self.flags.stop_reason.load(Ordering::Acquire) == REASON_NONE {
            self.flags
                .stop_reason
                .compare_exchange(
                    REASON_NONE,
                    REASON_USER_STOP,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .ok();
        }
        self.flags.active.store(false, Ordering::Release);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.sender
            .send(WorkerCommand::Stop { reply: reply_tx })
            .map_err(|_| "The recording worker is not available.".to_string())?;
        reply_rx
            .recv_timeout(STOP_TIMEOUT)
            .map_err(|_| "The recording worker did not finish the recording in time.".to_string())?
    }

    pub(crate) fn snapshot(&self) -> Result<RecordingSnapshot, String> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| "Recording status is unavailable.".to_string())
    }
}

impl Drop for RecordingHub {
    fn drop(&mut self) {
        let _ = self.sender.send(WorkerCommand::Shutdown);
        if let Ok(worker) = self.worker.get_mut() {
            if let Some(worker) = worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "GetCurrentThread"]
    fn get_current_thread() -> *mut std::ffi::c_void;
    #[link_name = "SetThreadPriority"]
    fn set_thread_priority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
}

#[cfg(target_os = "windows")]
fn lower_worker_priority() {
    // Best-effort only: recording stays correct if the host denies this hint.
    unsafe {
        let _ = set_thread_priority(get_current_thread(), -1);
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn lower_worker_priority() {
    // Nice +10 reduces contention with the native CPAL callbacks and the UI.
    unsafe {
        let _ = libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as u32, 10);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "android", target_os = "linux")))]
fn lower_worker_priority() {}

enum WorkerCommand {
    Attach {
        consumer: Consumer<CapturedFrame>,
    },
    Detach,
    Start {
        path: PathBuf,
        format: RecordingFormat,
        sample_rate: u32,
        start_ns: u64,
        license_deadline_ns: u64,
        reply: SyncSender<Result<(), String>>,
    },
    Stop {
        reply: SyncSender<Result<RecordingFile, String>>,
    },
    Shutdown,
}

#[derive(Default)]
struct MixSlot {
    left: f64,
    right: f64,
}

enum PcmStageWriter {
    Wav(hound::WavWriter<BufWriter<File>>),
    Raw16(BufWriter<File>),
    Mp3(Mp3RecordingEncoder),
    Ogg(OggRecordingEncoder),
    M4a(AacMp4RecordingEncoder),
}

impl PcmStageWriter {
    fn write_input_frame(&mut self, frame: StereoFrame, bit_depth: u16) -> Result<u64, String> {
        match self {
            Self::Wav(writer) => {
                let left = quantize(frame.left, bit_depth);
                let right = quantize(frame.right, bit_depth);
                writer
                    .write_sample(left)
                    .and_then(|()| writer.write_sample(right))
                    .map_err(|error| format!("Could not write the WAV recording: {error}"))?;
                Ok(1)
            }
            Self::Raw16(writer) => {
                let left = quantize(frame.left, 16) as i16;
                let right = quantize(frame.right, 16) as i16;
                writer
                    .write_all(&left.to_le_bytes())
                    .and_then(|()| writer.write_all(&right.to_le_bytes()))
                    .map_err(|error| format!("Could not stage PCM for FLAC export: {error}"))?;
                Ok(1)
            }
            Self::Mp3(writer) => writer.write_input_frame(frame),
            Self::Ogg(writer) => writer.write_input_frame(frame),
            Self::M4a(writer) => writer.write_input_frame(frame),
        }
    }
}

/// Bounded, causal stereo resampler used only on the background recording worker.
struct StreamingPcmResampler {
    input_rate: u32,
    output_rate: u32,
    input_frames: u64,
    base_index: u64,
    next_output_position: f64,
    frames: VecDeque<StereoFrame>,
}

impl StreamingPcmResampler {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate: input_rate.max(1),
            output_rate: output_rate.max(1),
            input_frames: 0,
            base_index: 0,
            next_output_position: 0.0,
            frames: VecDeque::with_capacity(16),
        }
    }

    fn push(&mut self, frame: StereoFrame) -> ([StereoFrame; 8], usize) {
        self.frames.push_back(frame);
        self.input_frames = self.input_frames.saturating_add(1);
        self.generate(false)
    }

    fn finish(&mut self) -> ([StereoFrame; 8], usize) {
        self.generate(true)
    }

    fn generate(&mut self, finalizing: bool) -> ([StereoFrame; 8], usize) {
        let mut output = [StereoFrame::default(); 8];
        let mut output_count = 0;
        loop {
            let first_index = self.next_output_position.floor().max(0.0) as u64;
            if finalizing {
                if self.next_output_position >= self.input_frames as f64 {
                    break;
                }
            } else if first_index.saturating_add(1) >= self.input_frames {
                break;
            }
            if output_count == output.len() {
                break;
            }
            let first = self.frame_at(first_index).unwrap_or_default();
            let second = self
                .frame_at(first_index.saturating_add(1))
                .unwrap_or(first);
            let fraction = (self.next_output_position - first_index as f64) as f32;
            output[output_count] = StereoFrame {
                left: first.left + (second.left - first.left) * fraction,
                right: first.right + (second.right - first.right) * fraction,
            };
            output_count += 1;
            self.next_output_position += f64::from(self.input_rate) / f64::from(self.output_rate);
        }

        if !finalizing && self.input_frames > 1 {
            let earliest_needed = (self.next_output_position.floor().max(0.0) as u64)
                .min(self.input_frames.saturating_sub(1));
            while self.base_index < earliest_needed {
                self.frames.pop_front();
                self.base_index = self.base_index.saturating_add(1);
            }
        }
        (output, output_count)
    }

    fn frame_at(&self, index: u64) -> Option<StereoFrame> {
        let offset = usize::try_from(index.checked_sub(self.base_index)?).ok()?;
        self.frames.get(offset).copied()
    }
}

fn output_frames(
    resampler: &mut Option<StreamingPcmResampler>,
    frame: StereoFrame,
) -> ([StereoFrame; 8], usize) {
    if let Some(resampler) = resampler {
        resampler.push(frame)
    } else {
        let mut frames = [StereoFrame::default(); 8];
        frames[0] = frame;
        (frames, 1)
    }
}

struct Mp3RecordingEncoder {
    encoder: shine_rs::Mp3Encoder,
    sink: BufWriter<File>,
    resampler: Option<StreamingPcmResampler>,
    pcm: Vec<i16>,
}

impl Mp3RecordingEncoder {
    fn new(file: File, input_rate: u32, output_rate: u32) -> Result<Self, String> {
        let config = shine_rs::Mp3EncoderConfig::new()
            .sample_rate(output_rate)
            .bitrate(320)
            .channels(2)
            .stereo_mode(shine_rs::StereoMode::JointStereo);
        let encoder = shine_rs::Mp3Encoder::new(config)
            .map_err(|error| format!("Could not initialize the MP3-320 encoder: {error}"))?;
        Ok(Self {
            encoder,
            sink: BufWriter::new(file),
            resampler: (input_rate != output_rate)
                .then(|| StreamingPcmResampler::new(input_rate, output_rate)),
            pcm: Vec::with_capacity(4_608),
        })
    }

    fn write_input_frame(&mut self, frame: StereoFrame) -> Result<u64, String> {
        let (frames, count) = output_frames(&mut self.resampler, frame);
        for frame in frames.into_iter().take(count) {
            self.write_output_frame(frame)?;
        }
        Ok(count as u64)
    }

    fn write_output_frame(&mut self, frame: StereoFrame) -> Result<(), String> {
        self.pcm.push(quantize(frame.left, 16) as i16);
        self.pcm.push(quantize(frame.right, 16) as i16);
        let samples_per_frame = self.encoder.samples_per_frame();
        while self.pcm.len() >= samples_per_frame {
            let encoded = self
                .encoder
                .encode_interleaved(&self.pcm[..samples_per_frame])
                .map_err(|error| format!("MP3 encoding failed: {error}"))?;
            self.pcm.drain(..samples_per_frame);
            for frame in encoded {
                self.sink
                    .write_all(&frame)
                    .map_err(|error| format!("Could not write MP3 data: {error}"))?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64, String> {
        let mut flushed_frames = 0_u64;
        if let Some(resampler) = self.resampler.as_mut() {
            let (frames, count) = resampler.finish();
            flushed_frames = count as u64;
            for frame in frames.into_iter().take(count) {
                self.write_output_frame(frame)?;
            }
        }
        if !self.pcm.is_empty() {
            let encoded = self
                .encoder
                .encode_interleaved(&self.pcm)
                .map_err(|error| format!("MP3 final-frame encoding failed: {error}"))?;
            self.pcm.clear();
            for frame in encoded {
                self.sink
                    .write_all(&frame)
                    .map_err(|error| format!("Could not write MP3 data: {error}"))?;
            }
        }
        let tail = self
            .encoder
            .finish()
            .map_err(|error| format!("Could not finalize MP3 stream: {error}"))?;
        self.sink
            .write_all(&tail)
            .and_then(|()| self.sink.flush())
            .map(|()| flushed_frames)
            .map_err(|error| format!("Could not flush MP3 stream: {error}"))
    }
}

struct OggRecordingEncoder {
    encoder: vorbis_rs::VorbisEncoder<BufWriter<File>>,
    resampler: Option<StreamingPcmResampler>,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl OggRecordingEncoder {
    fn new(file: File, input_rate: u32, output_rate: u32) -> Result<Self, String> {
        let sink = BufWriter::new(file);
        let mut builder = vorbis_rs::VorbisEncoderBuilder::new_with_serial(
            std::num::NonZeroU32::new(output_rate)
                .ok_or_else(|| "OGG output sample rate must be nonzero".to_owned())?,
            std::num::NonZeroU8::new(2).ok_or_else(|| "OGG output must be stereo".to_owned())?,
            sink,
            NEXT_OGG_SERIAL.fetch_add(1, Ordering::Relaxed) as i32,
        );
        builder.bitrate_management_strategy(
            vorbis_rs::VorbisBitrateManagementStrategy::QualityVbr {
                target_quality: 0.45,
            },
        );
        let encoder = builder
            .build()
            .map_err(|error| format!("Could not initialize OGG/Vorbis encoder: {error}"))?;
        Ok(Self {
            encoder,
            resampler: (input_rate != output_rate)
                .then(|| StreamingPcmResampler::new(input_rate, output_rate)),
            left: Vec::with_capacity(1_024),
            right: Vec::with_capacity(1_024),
        })
    }

    fn write_input_frame(&mut self, frame: StereoFrame) -> Result<u64, String> {
        let (frames, count) = output_frames(&mut self.resampler, frame);
        for frame in frames.into_iter().take(count) {
            self.left.push(finite_sample(frame.left));
            self.right.push(finite_sample(frame.right));
            if self.left.len() >= 1_024 {
                self.encode_block()?;
            }
        }
        Ok(count as u64)
    }

    fn encode_block(&mut self) -> Result<(), String> {
        if self.left.is_empty() {
            return Ok(());
        }
        let planar = [&self.left[..], &self.right[..]];
        self.encoder
            .encode_audio_block(planar)
            .map_err(|error| format!("OGG/Vorbis encoding failed: {error}"))?;
        self.left.clear();
        self.right.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<u64, String> {
        let mut flushed_frames = 0_u64;
        if let Some(resampler) = self.resampler.as_mut() {
            let (frames, count) = resampler.finish();
            flushed_frames = count as u64;
            for frame in frames.into_iter().take(count) {
                self.left.push(finite_sample(frame.left));
                self.right.push(finite_sample(frame.right));
            }
        }
        self.encode_block()?;
        let mut sink = self
            .encoder
            .finish()
            .map_err(|error| format!("Could not finalize OGG/Vorbis stream: {error}"))?;
        sink.flush()
            .map(|()| flushed_frames)
            .map_err(|error| format!("Could not flush OGG/Vorbis stream: {error}"))
    }
}

struct AacMp4RecordingEncoder {
    encoder: rusty_aac::encode::AacEncoder,
    writer: mp4::Mp4Writer<File>,
    resampler: Option<StreamingPcmResampler>,
    output_rate: u32,
    pcm: Vec<f32>,
    next_pts: u64,
}

impl AacMp4RecordingEncoder {
    const BITRATE_BPS: u32 = 160_000;

    fn new(file: File, input_rate: u32, output_rate: u32) -> Result<Self, String> {
        let brands = vec![
            mp4::FourCC { value: *b"M4A " },
            mp4::FourCC { value: *b"mp42" },
            mp4::FourCC { value: *b"isom" },
        ];
        let config = mp4::Mp4Config {
            major_brand: mp4::FourCC { value: *b"M4A " },
            minor_version: 0x200,
            compatible_brands: brands,
            timescale: output_rate,
        };
        let mut writer = mp4::Mp4Writer::write_start(file, &config)
            .map_err(|error| format!("Could not initialize audio-only M4A container: {error}"))?;
        let mut track = mp4::TrackConfig::from(mp4::AacConfig {
            bitrate: Self::BITRATE_BPS,
            profile: mp4::AudioObjectType::AacLowComplexity,
            freq_index: mp4::SampleFreqIndex::Freq48000,
            chan_conf: mp4::ChannelConfig::Stereo,
        });
        track.timescale = output_rate;
        writer
            .add_track(&track)
            .map_err(|error| format!("Could not add AAC audio track to M4A: {error}"))?;
        let encoder = rusty_aac::encode::AacEncoder::new(rusty_aac::encode::AacEncoderConfig {
            bitrate_bps: Self::BITRATE_BPS,
            ..rusty_aac::encode::AacEncoderConfig::default()
        });
        Ok(Self {
            encoder,
            writer,
            resampler: (input_rate != output_rate)
                .then(|| StreamingPcmResampler::new(input_rate, output_rate)),
            output_rate,
            pcm: Vec::with_capacity(2_048),
            next_pts: 0,
        })
    }

    fn write_input_frame(&mut self, frame: StereoFrame) -> Result<u64, String> {
        let (frames, count) = output_frames(&mut self.resampler, frame);
        for frame in frames.into_iter().take(count) {
            self.pcm.push(finite_sample(frame.left).clamp(-1.0, 1.0));
            self.pcm.push(finite_sample(frame.right).clamp(-1.0, 1.0));
            if self.pcm.len() >= 2_048 {
                self.submit_pcm()?;
            }
        }
        Ok(count as u64)
    }

    fn submit_pcm(&mut self) -> Result<(), String> {
        if self.pcm.is_empty() {
            return Ok(());
        }
        self.encoder
            .push_pcm(&self.pcm, 2, self.output_rate)
            .map_err(|error| format!("AAC encoding failed: {error}"))?;
        self.pcm.clear();
        self.drain_packets()
    }

    fn drain_packets(&mut self) -> Result<(), String> {
        loop {
            let packet = match self.encoder.next_packet() {
                Ok(packet) => packet,
                Err(rusty_aac::Error::Again | rusty_aac::Error::Eof) => break,
                Err(error) => return Err(format!("Could not retrieve AAC packet: {error}")),
            };
            let start_time = u64::try_from(packet.pts)
                .map_err(|_| "AAC encoder produced a negative packet timestamp".to_owned())?;
            if start_time < self.next_pts {
                return Err("AAC encoder produced out-of-order packet timestamps".to_owned());
            }
            let duration = packet.duration.max(1);
            self.writer
                .write_sample(
                    1,
                    &mp4::Mp4Sample {
                        start_time,
                        duration,
                        rendering_offset: 0,
                        is_sync: true,
                        bytes: mp4::Bytes::from(packet.data),
                    },
                )
                .map_err(|error| format!("Could not mux AAC packet into M4A: {error}"))?;
            self.next_pts = start_time.saturating_add(u64::from(duration));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64, String> {
        let mut flushed_frames = 0_u64;
        if let Some(resampler) = self.resampler.as_mut() {
            let (frames, count) = resampler.finish();
            flushed_frames = count as u64;
            for frame in frames.into_iter().take(count) {
                self.pcm.push(finite_sample(frame.left).clamp(-1.0, 1.0));
                self.pcm.push(finite_sample(frame.right).clamp(-1.0, 1.0));
                if self.pcm.len() >= 2_048 {
                    self.submit_pcm()?;
                }
            }
        }
        self.submit_pcm()?;
        self.encoder.finish();
        self.drain_packets()?;
        self.writer
            .write_end()
            .map_err(|error| format!("Could not finalize M4A container: {error}"))?;
        let mut file = self.writer.into_writer();
        file.flush()
            .and_then(|()| file.sync_all())
            .map(|()| flushed_frames)
            .map_err(|error| format!("Could not flush completed M4A file: {error}"))
    }
}

static NEXT_OGG_SERIAL: AtomicU32 = AtomicU32::new(1);

struct ActiveWriter {
    output_path: PathBuf,
    partial_path: PathBuf,
    encoded_partial_path: PathBuf,
    format: RecordingFormat,
    input_sample_rate: u32,
    sample_rate: u32,
    start_ns: u64,
    deadline_ns: u64,
    writer: Option<PcmStageWriter>,
    frames_written: u64,
    next_frame: u64,
    max_frame: Option<u64>,
    last_master_frame: Option<u64>,
    pending: BTreeMap<u64, MixSlot>,
    stop_started_ns: Option<u64>,
}

fn worker_loop(
    receiver: Receiver<WorkerCommand>,
    flags: Arc<CaptureFlags>,
    clock: Arc<CaptureClock>,
    snapshot: Arc<Mutex<RecordingSnapshot>>,
) {
    let mut consumer: Option<Consumer<CapturedFrame>> = None;
    let mut writer: Option<ActiveWriter> = None;
    let mut pending_stop: Option<SyncSender<Result<RecordingFile, String>>> = None;
    let mut shutting_down = false;

    while !shutting_down {
        loop {
            match receiver.try_recv() {
                Ok(WorkerCommand::Attach {
                    consumer: new_consumer,
                }) => {
                    drain_one(&mut consumer, &mut writer, &flags);
                    consumer = Some(new_consumer);
                }
                Ok(WorkerCommand::Detach) => {
                    drain_one(&mut consumer, &mut writer, &flags);
                    consumer = None;
                }
                Ok(WorkerCommand::Start {
                    path,
                    format,
                    sample_rate,
                    start_ns,
                    license_deadline_ns,
                    reply,
                }) => {
                    if writer.is_some() {
                        let _ = reply.send(Err("A master recording is already running.".into()));
                    } else if consumer.is_none() {
                        let _ = reply.send(Err(
                            "Load a track on at least one mix deck before recording.".into(),
                        ));
                    } else {
                        match ActiveWriter::create(
                            path,
                            format,
                            sample_rate,
                            start_ns,
                            license_deadline_ns,
                        ) {
                            Ok(active) => {
                                writer = Some(active);
                                flags.dropped_frames.store(0, Ordering::Relaxed);
                                flags.stop_reason.store(REASON_NONE, Ordering::Relaxed);
                                if let Ok(mut current) = snapshot.lock() {
                                    *current = RecordingSnapshot {
                                        active: true,
                                        status_code: "recording.started".into(),
                                        format: Some(format.label().into()),
                                        sample_rate: Some(format.output_sample_rate(sample_rate)),
                                        bit_depth: Some(format.bits_per_sample()),
                                        path: writer
                                            .as_ref()
                                            .map(|active| active.output_path.display().to_string()),
                                        frames_written: 0,
                                        dropped_frames: 0,
                                        error: None,
                                    };
                                }
                                let _ = reply.send(Ok(()));
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                }
                Ok(WorkerCommand::Stop { reply }) => {
                    if writer.is_none() {
                        let reason = flags.stop_reason.load(Ordering::Acquire);
                        let error = if reason == REASON_LICENSE_EXPIRED {
                            "Recording aborted: the Pro license expired.".to_string()
                        } else if reason == REASON_RING_OVERFLOW {
                            "Recording aborted: the real-time capture buffer overflowed."
                                .to_string()
                        } else {
                            "There is no active master recording.".to_string()
                        };
                        let _ = reply.send(Err(error));
                    } else {
                        if let Some(active) = writer.as_mut() {
                            active.stop_started_ns = Some(clock.now_ns());
                        }
                        pending_stop = Some(reply);
                    }
                }
                Ok(WorkerCommand::Shutdown) => {
                    shutting_down = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    shutting_down = true;
                    break;
                }
            }
        }

        if shutting_down {
            break;
        }

        let received = drain_one(&mut consumer, &mut writer, &flags);

        let now_ns = clock.now_ns();
        if let Some(active) = writer.as_mut() {
            if flags.active.load(Ordering::Acquire)
                && now_ns >= active.deadline_ns
                && flags.stop_reason.load(Ordering::Acquire) == REASON_NONE
            {
                flags
                    .stop_reason
                    .compare_exchange(
                        REASON_NONE,
                        REASON_LICENSE_EXPIRED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .ok();
                flags.active.store(false, Ordering::Release);
            }
            let reason = flags.stop_reason.load(Ordering::Acquire);
            if reason == REASON_LICENSE_EXPIRED || reason == REASON_RING_OVERFLOW {
                abort_writer(&mut writer, &snapshot, &flags, reason);
                if let Some(reply) = pending_stop.take() {
                    let message = if reason == REASON_LICENSE_EXPIRED {
                        "Recording aborted: the Pro license expired.".to_string()
                    } else {
                        "Recording aborted: the real-time capture buffer overflowed.".to_string()
                    };
                    let _ = reply.send(Err(message));
                }
            } else if let Some(active) = writer.as_mut() {
                let now_frame = frame_at_time(active, now_ns);
                let safe_flush = now_frame.saturating_sub(ALIGNMENT_LOOKAHEAD_FRAMES);
                if let Err(error) = flush_until(active, safe_flush, &flags) {
                    abort_writer(&mut writer, &snapshot, &flags, REASON_RING_OVERFLOW);
                    if let Some(reply) = pending_stop.take() {
                        let _ = reply.send(Err(error));
                    }
                } else {
                    update_snapshot(&snapshot, active, &flags);
                    let stop_ready = active.stop_started_ns.is_some_and(|started| {
                        now_ns.saturating_sub(started) >= STOP_DRAIN_GRACE.as_nanos() as u64
                    });
                    if stop_ready {
                        if let Some(mut finished) = writer.take() {
                            // Include every audio frame already produced by the callbacks at stop,
                            // even if CPAL predicted its DAC playback slightly in the future.
                            let stop_frame = finished.max_frame.unwrap_or(0);
                            let partial_path = finished.partial_path.clone();
                            let encoded_partial_path = finished.encoded_partial_path.clone();
                            let flush_result = flush_until(&mut finished, stop_frame, &flags);
                            let result = flush_result.and_then(|()| finished.finish());
                            match result {
                                Ok(file) => {
                                    if let Ok(mut current) = snapshot.lock() {
                                        current.active = false;
                                        current.status_code = "recording.saved".into();
                                        current.path = Some(file.path.clone());
                                        current.frames_written = file.frames;
                                        current.dropped_frames =
                                            flags.dropped_frames.load(Ordering::Relaxed);
                                        current.error = None;
                                    }
                                    if let Some(reply) = pending_stop.take() {
                                        let _ = reply.send(Ok(file));
                                    }
                                }
                                Err(error) => {
                                    let _ = fs::remove_file(partial_path);
                                    let _ = fs::remove_file(encoded_partial_path);
                                    if let Ok(mut current) = snapshot.lock() {
                                        current.active = false;
                                        current.status_code = "recording.failed".into();
                                        current.error = Some(error.clone());
                                    }
                                    if let Some(reply) = pending_stop.take() {
                                        let _ = reply.send(Err(error));
                                    }
                                }
                            }
                        }
                    } else if !received {
                        match receiver.recv_timeout(WORKER_POLL) {
                            Ok(command) => {
                                // Preserve FIFO behavior by handling the command on the next pass.
                                // A one-slot queue avoids unbounded control-message accumulation.
                                handle_deferred_command(
                                    command,
                                    &mut consumer,
                                    &mut writer,
                                    &mut pending_stop,
                                    &flags,
                                    &clock,
                                    &snapshot,
                                    &mut shutting_down,
                                );
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => shutting_down = true,
                        }
                    }
                }
            }
        } else if !received {
            match receiver.recv_timeout(WORKER_POLL) {
                Ok(command) => handle_deferred_command(
                    command,
                    &mut consumer,
                    &mut writer,
                    &mut pending_stop,
                    &flags,
                    &clock,
                    &snapshot,
                    &mut shutting_down,
                ),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => shutting_down = true,
            }
        }
    }

    flags.active.store(false, Ordering::Release);
    if let Some(active) = writer.take() {
        drop(active.writer);
        let _ = fs::remove_file(active.partial_path);
        let _ = fs::remove_file(active.encoded_partial_path);
    }
    if let Some(reply) = pending_stop.take() {
        let _ = reply.send(Err(
            "The recording worker stopped before finalizing the file.".into(),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_deferred_command(
    command: WorkerCommand,
    consumer: &mut Option<Consumer<CapturedFrame>>,
    writer: &mut Option<ActiveWriter>,
    pending_stop: &mut Option<SyncSender<Result<RecordingFile, String>>>,
    flags: &CaptureFlags,
    clock: &CaptureClock,
    snapshot: &Mutex<RecordingSnapshot>,
    shutting_down: &mut bool,
) {
    match command {
        WorkerCommand::Attach {
            consumer: new_consumer,
        } => {
            drain_one(consumer, writer, flags);
            *consumer = Some(new_consumer);
        }
        WorkerCommand::Detach => {
            drain_one(consumer, writer, flags);
            *consumer = None;
        }
        WorkerCommand::Start {
            path,
            format,
            sample_rate,
            start_ns,
            license_deadline_ns,
            reply,
        } => {
            if writer.is_some() {
                let _ = reply.send(Err("A master recording is already running.".into()));
            } else if consumer.is_none() {
                let _ = reply.send(Err(
                    "Load a track on at least one mix deck before recording.".into(),
                ));
            } else {
                match ActiveWriter::create(path, format, sample_rate, start_ns, license_deadline_ns)
                {
                    Ok(active) => {
                        *writer = Some(active);
                        flags.dropped_frames.store(0, Ordering::Relaxed);
                        flags.stop_reason.store(REASON_NONE, Ordering::Relaxed);
                        if let Ok(mut current) = snapshot.lock() {
                            *current = RecordingSnapshot {
                                active: true,
                                status_code: "recording.started".into(),
                                format: Some(format.label().into()),
                                sample_rate: Some(format.output_sample_rate(sample_rate)),
                                bit_depth: Some(format.bits_per_sample()),
                                path: writer
                                    .as_ref()
                                    .map(|active| active.output_path.display().to_string()),
                                frames_written: 0,
                                dropped_frames: 0,
                                error: None,
                            };
                        }
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
        }
        WorkerCommand::Stop { reply } => {
            if let Some(active) = writer.as_mut() {
                active.stop_started_ns.get_or_insert_with(|| clock.now_ns());
                *pending_stop = Some(reply);
            } else {
                let _ = reply.send(Err("There is no active master recording.".into()));
            }
        }
        WorkerCommand::Shutdown => *shutting_down = true,
    }
}

fn drain_one(
    consumer: &mut Option<Consumer<CapturedFrame>>,
    writer: &mut Option<ActiveWriter>,
    flags: &CaptureFlags,
) -> bool {
    let Some(consumer) = consumer.as_mut() else {
        return false;
    };
    let mut received = false;
    for _ in 0..16_384 {
        let Ok(frame) = consumer.pop() else {
            break;
        };
        received = true;
        if let Some(active) = writer.as_mut() {
            if frame.timestamp_ns < active.start_ns || frame.timestamp_ns >= active.deadline_ns {
                continue;
            }
            let mut index = frame_at_time(active, frame.timestamp_ns);
            if let Some(previous) = active.last_master_frame {
                index = index.max(previous.saturating_add(1));
            }
            active.last_master_frame = Some(index);
            if index < active.next_frame {
                flags.record_drop();
                continue;
            }
            if index.saturating_sub(active.next_frame) > MAX_PENDING_FRAMES {
                flags.record_drop();
                continue;
            }
            // The ring carries the already-summed and limited master signal. Store exactly one
            // stereo sample per timeline slot; source decks are never re-summed in this worker.
            let slot = active.pending.entry(index).or_default();
            slot.left = finite_sample(frame.left) as f64;
            slot.right = finite_sample(frame.right) as f64;
            active.max_frame = Some(active.max_frame.map_or(index, |current| current.max(index)));
        }
    }
    received
}

fn frame_at_time(writer: &ActiveWriter, timestamp_ns: u64) -> u64 {
    let elapsed = timestamp_ns.saturating_sub(writer.start_ns) as u128;
    ((elapsed * writer.input_sample_rate as u128) / 1_000_000_000_u128).min(u64::MAX as u128) as u64
}

fn flush_until(
    writer: &mut ActiveWriter,
    inclusive_frame: u64,
    flags: &CaptureFlags,
) -> Result<(), String> {
    if writer.max_frame.is_none() && inclusive_frame == 0 {
        return Ok(());
    }
    let max_frame = writer.max_frame.unwrap_or(0);
    let last = inclusive_frame.min(max_frame);
    if last < writer.next_frame {
        return Ok(());
    }
    while writer.next_frame <= last {
        if flags.stop_reason.load(Ordering::Acquire) == REASON_LICENSE_EXPIRED
            || flags.stop_reason.load(Ordering::Acquire) == REASON_RING_OVERFLOW
        {
            return Err("Capture stopped before the writer could flush pending audio.".into());
        }
        let current_frame = writer.next_frame;
        let mixed = writer.pending.remove(&current_frame).unwrap_or_default();
        // Capture is already tapped after the shared master limiter. Do not introduce a second
        // recording-only limiter or deck summation stage here.
        let left = (mixed.left as f32).clamp(-1.0, 1.0);
        let right = (mixed.right as f32).clamp(-1.0, 1.0);
        let produced_frames = writer
            .writer
            .as_mut()
            .ok_or_else(|| "PCM writer was already finalized.".to_string())?
            .write_input_frame(StereoFrame { left, right }, writer.format.bits_per_sample())?;
        writer.frames_written = writer.frames_written.saturating_add(produced_frames);
        writer.next_frame = writer.next_frame.saturating_add(1);
    }
    Ok(())
}

fn quantize(sample: f32, bit_depth: u16) -> i32 {
    let scale = if bit_depth == 24 {
        8_388_607.0
    } else {
        32_767.0
    };
    (sample.clamp(-1.0, 1.0) * scale).round() as i32
}

fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

fn update_snapshot(
    snapshot: &Mutex<RecordingSnapshot>,
    writer: &ActiveWriter,
    flags: &CaptureFlags,
) {
    if let Ok(mut current) = snapshot.lock() {
        current.active = true;
        current.frames_written = writer.frames_written;
        current.dropped_frames = flags.dropped_frames.load(Ordering::Relaxed);
    }
}

fn abort_writer(
    writer: &mut Option<ActiveWriter>,
    snapshot: &Mutex<RecordingSnapshot>,
    flags: &CaptureFlags,
    reason: u8,
) {
    if let Some(active) = writer.take() {
        drop(active.writer);
        let _ = fs::remove_file(active.partial_path);
        let _ = fs::remove_file(active.encoded_partial_path);
    }
    if let Ok(mut current) = snapshot.lock() {
        current.active = false;
        current.status_code = if reason == REASON_LICENSE_EXPIRED {
            "recording.license_expired"
        } else {
            "recording.capture_overflow"
        }
        .into();
        current.dropped_frames = flags.dropped_frames.load(Ordering::Relaxed);
        current.error = Some(if reason == REASON_LICENSE_EXPIRED {
            "Pro license expired; the incomplete recording was deleted.".into()
        } else {
            "Capture buffer overflowed; the incomplete recording was deleted.".into()
        });
    }
}

impl ActiveWriter {
    fn create(
        output_path: PathBuf,
        format: RecordingFormat,
        input_sample_rate: u32,
        start_ns: u64,
        deadline_ns: u64,
    ) -> Result<Self, String> {
        if !(8_000..=192_000).contains(&input_sample_rate) {
            return Err("The active audio device has an unsupported recording sample rate.".into());
        }
        let sample_rate = format.output_sample_rate(input_sample_rate);
        let parent = output_path
            .parent()
            .ok_or_else(|| "The recording destination has no parent directory.".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create the recording folder: {error}"))?;
        let file_name = output_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "The recording file name is invalid.".to_string())?;
        if output_path.exists() {
            return Err("A recording with this file name already exists.".into());
        }
        let partial_path = output_path.with_file_name(format!(".{file_name}.partial"));
        let encoded_partial_path =
            output_path.with_file_name(format!(".{file_name}.encoded.partial"));
        if encoded_partial_path.exists() {
            return Err("A temporary recording file already exists.".into());
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial_path)
            .map_err(|error| format!("Could not create the recording file: {error}"))?;
        let writer_result = match format {
            RecordingFormat::WavPcm16 | RecordingFormat::WavPcm24 => {
                hound::WavWriter::new(BufWriter::new(file), format.hound_spec(sample_rate))
                    .map(PcmStageWriter::Wav)
                    .map_err(|error| format!("Could not initialize the WAV encoder: {error}"))
            }
            RecordingFormat::Flac => Ok(PcmStageWriter::Raw16(BufWriter::new(file))),
            RecordingFormat::Mp3_320 => {
                Mp3RecordingEncoder::new(file, input_sample_rate, sample_rate)
                    .map(PcmStageWriter::Mp3)
            }
            RecordingFormat::OggVorbis => {
                OggRecordingEncoder::new(file, input_sample_rate, sample_rate)
                    .map(PcmStageWriter::Ogg)
            }
            RecordingFormat::M4aAac => {
                AacMp4RecordingEncoder::new(file, input_sample_rate, sample_rate)
                    .map(PcmStageWriter::M4a)
            }
        };
        let writer = match writer_result {
            Ok(writer) => writer,
            Err(error) => {
                let _ = fs::remove_file(&partial_path);
                let _ = fs::remove_file(&encoded_partial_path);
                return Err(error);
            }
        };
        Ok(Self {
            output_path,
            partial_path,
            encoded_partial_path,
            format,
            input_sample_rate,
            sample_rate,
            start_ns,
            deadline_ns,
            writer: Some(writer),
            frames_written: 0,
            next_frame: 0,
            max_frame: None,
            last_master_frame: None,
            pending: BTreeMap::new(),
            stop_started_ns: None,
        })
    }

    fn finish(mut self) -> Result<RecordingFile, String> {
        if self.next_frame == 0 {
            return Err("No audio frames were captured; the recording was not saved.".into());
        }
        let writer = self
            .writer
            .take()
            .ok_or_else(|| "PCM writer was already finalized.".to_string())?;
        match writer {
            PcmStageWriter::Wav(wav) => {
                wav.finalize()
                    .map_err(|error| format!("Could not finalize the WAV recording: {error}"))?;
                sync_file(&self.partial_path, "WAV")?;
                fs::rename(&self.partial_path, &self.output_path).map_err(|error| {
                    format!("Could not publish the completed recording: {error}")
                })?;
            }
            PcmStageWriter::Raw16(mut raw) => {
                raw.flush()
                    .map_err(|error| format!("Could not flush staged PCM for FLAC: {error}"))?;
                drop(raw);
                crate::import::separator::encode_recording_pcm16_to_flac(
                    &self.partial_path,
                    &self.encoded_partial_path,
                    self.sample_rate,
                    self.frames_written,
                )
                .map_err(|error| format!("Could not encode FLAC recording: {error:#}"))?;
                sync_file(&self.encoded_partial_path, "FLAC")?;
                fs::rename(&self.encoded_partial_path, &self.output_path).map_err(|error| {
                    format!("Could not publish the completed FLAC recording: {error}")
                })?;
                let _ = fs::remove_file(&self.partial_path);
            }
            PcmStageWriter::Mp3(mp3) => {
                self.frames_written = self.frames_written.saturating_add(mp3.finish()?);
                sync_file(&self.partial_path, "MP3")?;
                fs::rename(&self.partial_path, &self.output_path).map_err(|error| {
                    format!("Could not publish the completed MP3 recording: {error}")
                })?;
            }
            PcmStageWriter::Ogg(ogg) => {
                self.frames_written = self.frames_written.saturating_add(ogg.finish()?);
                sync_file(&self.partial_path, "OGG/Vorbis")?;
                fs::rename(&self.partial_path, &self.output_path).map_err(|error| {
                    format!("Could not publish the completed OGG recording: {error}")
                })?;
            }
            PcmStageWriter::M4a(m4a) => {
                self.frames_written = self.frames_written.saturating_add(m4a.finish()?);
                sync_file(&self.partial_path, "M4A/AAC")?;
                fs::rename(&self.partial_path, &self.output_path).map_err(|error| {
                    format!("Could not publish the completed M4A recording: {error}")
                })?;
            }
        }
        if self.frames_written == 0 {
            let _ = fs::remove_file(&self.output_path);
            return Err(
                "The encoder produced no audio frames; the recording was not saved.".into(),
            );
        }
        let metadata = fs::metadata(&self.output_path)
            .map_err(|error| format!("Could not read the completed recording: {error}"))?;
        let name = self
            .output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("master-recording")
            .to_string();
        Ok(RecordingFile {
            name,
            path: self.output_path.display().to_string(),
            format: self.format.label().into(),
            sample_rate: self.sample_rate,
            channels: 2,
            bit_depth: self.format.bits_per_sample(),
            frames: self.frames_written,
            size_bytes: metadata.len(),
        })
    }
}

fn sync_file(path: &std::path::Path, format: &str) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("Could not sync completed {format} recording: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn pcm_quantization_uses_signed_integer_ranges() {
        assert_eq!(quantize(1.0, 16), i16::MAX as i32);
        assert_eq!(quantize(-1.0, 16), -(i16::MAX as i32));
        assert_eq!(quantize(1.0, 24), 8_388_607);
        assert_eq!(quantize(f32::NAN, 24), 0);
    }

    #[test]
    fn capture_gate_stops_at_license_deadline() {
        let flags = CaptureFlags::new();
        flags.start_ns.store(10, Ordering::Relaxed);
        flags.deadline_ns.store(20, Ordering::Relaxed);
        flags.active.store(true, Ordering::Release);
        assert!(!flags.should_capture(9));
        assert!(flags.should_capture(19));
        assert!(!flags.should_capture(20));
        assert_eq!(
            flags.stop_reason.load(Ordering::Relaxed),
            REASON_LICENSE_EXPIRED
        );
        assert!(!flags.active.load(Ordering::Acquire));
    }

    #[test]
    fn master_writer_records_the_post_mix_master_without_resumming_decks() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after Unix epoch")
            .as_nanos();
        let unique = format!(
            "championvibe-recording-test-{}-{timestamp}.wav",
            std::process::id()
        );
        let path = std::env::temp_dir().join(unique);
        let hub = RecordingHub::new().expect("recording worker starts");
        let (mut master, consumer) = hub.new_capture_ring();
        hub.attach(consumer, 48_000)
            .expect("attach post-mix master tap");
        hub.start(path.clone(), RecordingFormat::WavPcm16, 48_000, u64::MAX)
            .expect("start master recording");

        let start_ns = hub.flags.start_ns.load(Ordering::Acquire);
        for frame_index in 0..128_u64 {
            let timestamp_ns = start_ns.saturating_add(
                ((frame_index as u128 * 1_000_000_000_u128) / 48_000).min(u64::MAX as u128) as u64,
            );
            // This is the post-summation/post-limiter master sample. The recorder must
            // serialize it as-is rather than summing the source deck taps a second time.
            master
                .push(CapturedFrame {
                    timestamp_ns,
                    left: 0.5,
                    right: -0.5,
                })
                .expect("master ring has room");
        }
        let file = hub.stop().expect("stop and finalize master WAV");
        assert_eq!(file.frames, 128);
        let mut reader = hound::WavReader::open(&path).expect("final WAV is readable");
        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|sample| sample.expect("valid PCM sample"))
            .collect();
        assert_eq!(samples.len(), 256);
        assert_eq!(samples[0], quantize(0.5, 16) as i16);
        assert_eq!(samples[1], quantize(-0.5, 16) as i16);
        drop(hub);
        fs::remove_file(path).expect("remove temporary WAV");
    }

    #[test]
    fn master_writer_exports_flac_from_background_pcm_stage() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "championvibe-recording-test-{}-{timestamp}.flac",
            std::process::id()
        ));
        let hub = RecordingHub::new().expect("recording worker starts");
        let (mut producer, consumer) = hub.new_capture_ring();
        hub.attach(consumer, 44_100).expect("attach deck");
        hub.start(path.clone(), RecordingFormat::Flac, 44_100, u64::MAX)
            .expect("start FLAC master recording");
        let start_ns = hub.flags.start_ns.load(Ordering::Acquire);
        for frame_index in 0..128_u64 {
            let timestamp_ns = start_ns.saturating_add(
                ((frame_index as u128 * 1_000_000_000_u128) / 44_100).min(u64::MAX as u128) as u64,
            );
            producer
                .push(CapturedFrame {
                    timestamp_ns,
                    left: 0.2,
                    right: -0.2,
                })
                .expect("capture ring has room");
        }
        let file = hub.stop().expect("stop and encode FLAC");
        assert_eq!(file.format, "flac");
        assert_eq!(file.frames, 128);
        let mut flac = File::open(&path).expect("final FLAC is readable");
        let mut signature = [0_u8; 4];
        flac.read_exact(&mut signature)
            .expect("FLAC signature exists");
        assert_eq!(&signature, b"fLaC");
        drop(hub);
        fs::remove_file(path).expect("remove temporary FLAC");
    }

    #[test]
    fn compressed_recording_encoders_write_recognizable_final_files() {
        let formats = [
            RecordingFormat::Mp3_320,
            RecordingFormat::OggVorbis,
            RecordingFormat::M4aAac,
        ];
        for format in formats {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time is after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "championvibe-recording-test-{}-{timestamp}.{}",
                std::process::id(),
                format.extension()
            ));
            let hub = RecordingHub::new().expect("recording worker starts");
            let (mut producer, consumer) = hub.new_capture_ring();
            hub.attach(consumer, 48_000)
                .expect("attach post-master tap");
            let started = hub
                .start(path.clone(), format, 48_000, u64::MAX)
                .expect("start compressed master recording");
            assert_eq!(started.sample_rate, Some(format.output_sample_rate(48_000)));

            let start_ns = hub.flags.start_ns.load(Ordering::Acquire);
            for frame_index in 0..8_192_u64 {
                let timestamp_ns = start_ns.saturating_add(
                    ((frame_index as u128 * 1_000_000_000_u128) / 48_000).min(u64::MAX as u128)
                        as u64,
                );
                let phase = frame_index as f32 * 0.013;
                producer
                    .push(CapturedFrame {
                        timestamp_ns,
                        left: phase.sin() * 0.2,
                        right: phase.cos() * 0.2,
                    })
                    .expect("master capture ring has room");
            }
            let file = hub.stop().expect("encoder finalizes a playable file");
            assert_eq!(file.format, format.label());
            assert_eq!(file.sample_rate, format.output_sample_rate(48_000));
            assert!(file.frames > 0);
            drop(hub);

            let bytes = fs::read(&path).expect("final encoded file is readable");
            assert!(
                bytes.len() > 128,
                "{} output should not be empty",
                format.label()
            );
            match format {
                RecordingFormat::Mp3_320 => {
                    let contains_320kbps_mpeg1_layer3_frame = bytes.windows(4).any(|header| {
                        header[0] == 0xff
                            && header[1] & 0xe0 == 0xe0
                            && (header[1] >> 3) & 0x03 == 0x03
                            && (header[1] >> 1) & 0x03 == 0x01
                            && (header[2] >> 4) & 0x0f == 0x0e
                    });
                    assert!(
                        contains_320kbps_mpeg1_layer3_frame,
                        "MP3 frame header missing"
                    );
                }
                RecordingFormat::OggVorbis => {
                    assert!(
                        bytes.starts_with(b"OggS"),
                        "OGG page capture pattern missing"
                    );
                    assert!(bytes.windows(7).any(|window| window == b"\x01vorbis"));
                }
                RecordingFormat::M4aAac => {
                    assert!(
                        bytes.windows(4).any(|window| window == b"ftyp"),
                        "MP4 ftyp box missing"
                    );
                    assert!(
                        bytes.windows(4).any(|window| window == b"mdat"),
                        "MP4 mdat box missing"
                    );
                    assert!(
                        bytes.windows(4).any(|window| window == b"moov"),
                        "MP4 moov box missing"
                    );
                    let mp4_reader = mp4::Mp4Reader::read_header(
                        File::open(&path).expect("M4A file opens"),
                        file.size_bytes,
                    )
                    .expect("M4A container parses");
                    assert_eq!(mp4_reader.tracks().len(), 1);
                    assert!(mp4_reader.sample_count(1).expect("AAC sample count parses") > 0);
                }
                _ => unreachable!("only compressed formats are listed here"),
            }
            fs::remove_file(path).expect("remove temporary encoded file");
        }
    }

    #[test]
    fn recording_format_parser_accepts_only_implemented_format_families() {
        assert_eq!(RecordingFormat::parse(None), Ok(RecordingFormat::WavPcm16));
        assert_eq!(
            RecordingFormat::parse(Some("wavPcm16")),
            Ok(RecordingFormat::WavPcm16)
        );
        assert_eq!(
            RecordingFormat::parse(Some("wavPcm24")),
            Ok(RecordingFormat::WavPcm24)
        );
        assert_eq!(
            RecordingFormat::parse(Some("flac")),
            Ok(RecordingFormat::Flac)
        );
        assert_eq!(
            RecordingFormat::parse(Some("mp3")),
            Ok(RecordingFormat::Mp3_320)
        );
        assert_eq!(
            RecordingFormat::parse(Some("oggVorbis")),
            Ok(RecordingFormat::OggVorbis)
        );
        assert_eq!(
            RecordingFormat::parse(Some("m4a")),
            Ok(RecordingFormat::M4aAac)
        );
        assert!(RecordingFormat::parse(Some("opus")).is_err());
    }

    #[test]
    fn timestamp_mapping_is_sample_rate_based() {
        let active = ActiveWriter {
            output_path: PathBuf::new(),
            partial_path: PathBuf::new(),
            encoded_partial_path: PathBuf::new(),
            format: RecordingFormat::WavPcm16,
            input_sample_rate: 48_000,
            sample_rate: 48_000,
            start_ns: 1_000_000_000,
            deadline_ns: u64::MAX,
            writer: None,
            frames_written: 0,
            next_frame: 0,
            max_frame: None,
            last_master_frame: None,
            pending: BTreeMap::new(),
            stop_started_ns: None,
        };
        assert_eq!(frame_at_time(&active, 1_000_020_833), 0);
        assert_eq!(frame_at_time(&active, 1_000_041_666), 1);
        assert_eq!(frame_at_time(&active, 2_000_000_000), 48_000);
    }
}
