#[cfg(target_os = "android")]
mod android_context;
mod auto_gain;
mod decoder;
pub(crate) mod dj_engine;
mod dsp;
mod limiter;
pub(crate) mod loudness;
mod master_bus;
pub(crate) mod mir_analysis;
pub(crate) mod model_manager;
pub(crate) mod onnx_embedding;
mod recording;
mod recording_formats;
mod samples;
mod wsola;

pub(crate) use recording::{
    CaptureClock, CaptureFlags, CapturedFrame, RecordingFile, RecordingFormat, RecordingHub,
    RecordingSnapshot,
};
pub(crate) use recording_formats::RecordingFormatInfo;

pub(crate) fn recording_formats() -> Vec<RecordingFormatInfo> {
    recording_formats::formats()
}

use arc_swap::ArcSwap;
use auto_gain::AutoGainRamp;
use dsp::DspChain;
use master_bus::{DeckOutput, MasterBus};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
pub use samples::SampleInfo;
use samples::{SampleBank, SampleClip};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use wsola::SharedSchedule;

const STEM_COUNT: usize = 4;
pub(crate) const MAX_TTS_PCM_BYTES: usize = 64 * 1024 * 1024;
const RING_CAPACITY_FRAMES: usize = 65_536;
const PREBUFFER_FRAMES: usize = 8_192;
const MAX_TTS_CLIP_DURATION_SECONDS: usize = 120;
const SAMPLE_EVENT_CAPACITY: usize = 256;
const PENDING_SAMPLE_EVENT_CAPACITY: usize = SAMPLE_EVENT_CAPACITY * 2;
const MAX_ACTIVE_SAMPLES: usize = 8;
const REWIND_DURATION_SECONDS: f64 = 0.8;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

/// Builds or reuses a binary packet for the currently requested track only.
/// The packet contains peak buckets and beat times, avoiding JSON number arrays over IPC.
pub fn waveform_packet(
    path: &std::path::Path,
    duration_ms: Option<i64>,
    bpm: Option<f64>,
    beat_times_seconds: &[f64],
    peak_count: usize,
) -> Result<Vec<u8>, String> {
    decoder::waveform_packet(path, duration_ms, bpm, beat_times_seconds, peak_count)
}

pub fn waveform_packet_from_peak_blob(
    peaks_blob: &[u8],
    bpm: Option<f64>,
    beat_times_seconds: &[f64],
    peak_count: usize,
) -> Result<Vec<u8>, String> {
    decoder::waveform_packet_from_peak_blob(peaks_blob, bpm, beat_times_seconds, peak_count)
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BeatGrid {
    #[serde(default)]
    pub beat_times_seconds: Vec<f64>,
    pub bpm: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SampleQuantization {
    Off,
    Beat,
    Bar,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsAudioSchedule {
    pub target_deck_frame: u64,
    pub delay_frames: u64,
    pub output_sample_rate: u32,
    pub quantization: String,
}

/// Decode the Tauri raw-IPC PCM format: interleaved IEEE-754 float32 little-endian, mono or stereo.
pub(crate) fn decode_tts_pcm_f32le(
    pcm: &[u8],
    sample_rate: u32,
    channels: u8,
) -> Result<Vec<StereoFrame>, String> {
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err("TTS PCM sample rate must be between 8 kHz and 192 kHz".to_owned());
    }
    if !matches!(channels, 1 | 2) {
        return Err("TTS PCM must have one or two interleaved channels".to_owned());
    }
    if pcm.is_empty() || pcm.len() > master_bus::MAX_MASTER_TTS_BYTES {
        return Err("TTS PCM is empty or exceeds the 64 MiB binary IPC limit".to_owned());
    }

    let bytes_per_frame = std::mem::size_of::<f32>() * usize::from(channels);
    if pcm.len() % bytes_per_frame != 0 {
        return Err("TTS PCM byte length is not aligned to complete float32 frames".to_owned());
    }
    let frame_count = pcm.len() / bytes_per_frame;
    if frame_count > (sample_rate as usize).saturating_mul(MAX_TTS_CLIP_DURATION_SECONDS) {
        return Err("TTS PCM exceeds the 120-second local playback limit".to_owned());
    }
    if frame_count.saturating_mul(std::mem::size_of::<StereoFrame>())
        > master_bus::MAX_MASTER_TTS_BYTES
    {
        return Err("decoded TTS PCM exceeds the 64 MiB master-aux memory limit".to_owned());
    }

    let mut frames = Vec::with_capacity(frame_count);
    for frame in pcm.chunks_exact(bytes_per_frame) {
        let left = f32::from_le_bytes(frame[..4].try_into().expect("validated float32 width"));
        let right = if channels == 1 {
            left
        } else {
            f32::from_le_bytes(
                frame[4..8]
                    .try_into()
                    .expect("validated stereo frame width"),
            )
        };
        if !left.is_finite() || !right.is_finite() || left.abs() > 8.0 || right.abs() > 8.0 {
            return Err("TTS PCM contains non-finite or unsafe over-range samples".to_owned());
        }
        frames.push(StereoFrame { left, right });
    }
    Ok(frames)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct StereoFrame {
    pub left: f32,
    pub right: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PitchFrame {
    pub wsola: StereoFrame,
    pub varispeed: StereoFrame,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DeckFrame {
    pub wsola_audio: StereoFrame,
    pub varispeed_audio: StereoFrame,
    /// Quantized sample-trigger bus, kept independent until the shared master mixer.
    pub sample_audio: StereoFrame,
    /// Local TTS shoutouts/dubplates have their own post-deck auxiliary input.
    pub tts_audio: StereoFrame,
    pub advances_timeline: bool,
}

#[derive(Clone, Copy)]
pub(super) struct AudioParameters {
    master_gain: f32,
    stem_gains: [f32; STEM_COUNT],
    high_pass_enabled: bool,
    high_pass_hz: f32,
    low_pass_enabled: bool,
    low_pass_hz: f32,
    bass_gain: f32,
    mid_gain: f32,
    high_gain: f32,
    kill_bass: bool,
    kill_mid: bool,
    kill_high: bool,
    tape_delay_ms: f32,
    tape_feedback: f32,
    tape_mix: f32,
    spring_mix: f32,
    spring_decay: f32,
    auto_gain_enabled: bool,
    auto_gain_target_lufs: f32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AudioControlUpdate {
    pub master_gain: Option<f32>,
    pub stem_gains: Option<[f32; STEM_COUNT]>,
    pub high_pass_enabled: Option<bool>,
    pub high_pass_hz: Option<f32>,
    pub low_pass_enabled: Option<bool>,
    pub low_pass_hz: Option<f32>,
    pub bass_gain: Option<f32>,
    pub mid_gain: Option<f32>,
    pub high_gain: Option<f32>,
    pub kill_bass: Option<bool>,
    pub kill_mid: Option<bool>,
    pub kill_high: Option<bool>,
    pub tape_delay_ms: Option<f32>,
    pub tape_feedback: Option<f32>,
    pub tape_mix: Option<f32>,
    pub spring_mix: Option<f32>,
    pub spring_decay: Option<f32>,
    /// Enable per-track R128 compensation. Tracks without a stored integrated-LUFS value are left unchanged.
    pub auto_gain_enabled: Option<bool>,
    /// Adjustable integrated-loudness target in LUFS; accepted range is -36 to -6.
    pub auto_gain_target_lufs: Option<f32>,
    /// Playback speed multiplier for the next load. Accepted range is 0.80..=1.20.
    pub playback_rate: Option<f32>,
    /// Toggle the live pitch-locked WSOLA branch. Existing decks crossfade to/from varispeed over 10 ms.
    pub pitch_lock_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub loaded: bool,
    pub playing: bool,
    pub finished: bool,
    pub failed: bool,
    pub backend: &'static str,
    pub sample_rate: u32,
    /// Active playback profile for this track: two (vocals/instrumental) or four stems.
    pub active_stems: u8,
    pub playback_rate: f32,
    /// Finite shared-master lookahead latency in milliseconds; zero before the output opens.
    pub limiter_latency_ms: f32,
    /// Current gain reduction applied by the post-sum master limiter, in dB.
    pub master_gain_reduction_db: f32,
    pub position_frames: u64,
    pub underruns: u64,
    pub device_errors: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StemFiles {
    pub acapella: PathBuf,
    pub instrumental: PathBuf,
    pub bass: PathBuf,
    pub drums: PathBuf,
}

impl StemFiles {
    fn into_ordered_paths(self) -> [PathBuf; STEM_COUNT] {
        [self.acapella, self.instrumental, self.bass, self.drums]
    }
}

struct AudioControls {
    playing: AtomicBool,
    auto_rewind: AtomicBool,
    active_stem_count: AtomicU32,
    sample_bank: Arc<ArcSwap<SampleBank>>,
    sample_bank_update: Mutex<()>,
    retired_sample_frames: Mutex<Vec<Arc<[StereoFrame]>>>,
    master_gain: AtomicU32,
    playback_rate: AtomicU32,
    pitch_lock_enabled: Arc<AtomicBool>,
    stem_gains: [AtomicU32; STEM_COUNT],
    high_pass_enabled: AtomicBool,
    high_pass_hz: AtomicU32,
    low_pass_enabled: AtomicBool,
    low_pass_hz: AtomicU32,
    bass_gain: AtomicU32,
    mid_gain: AtomicU32,
    high_gain: AtomicU32,
    kill_bass: AtomicBool,
    kill_mid: AtomicBool,
    kill_high: AtomicBool,
    tape_delay_ms: AtomicU32,
    tape_feedback: AtomicU32,
    tape_mix: AtomicU32,
    spring_mix: AtomicU32,
    spring_decay: AtomicU32,
    auto_gain_enabled: AtomicBool,
    auto_gain_target_lufs: AtomicU32,
}

impl AudioControls {
    fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            auto_rewind: AtomicBool::new(false),
            active_stem_count: AtomicU32::new(4),
            sample_bank: Arc::new(ArcSwap::from(Arc::new(SampleBank::builtins()))),
            sample_bank_update: Mutex::new(()),
            retired_sample_frames: Mutex::new(Vec::new()),
            master_gain: atomic_f32(0.75),
            playback_rate: atomic_f32(1.0),
            pitch_lock_enabled: Arc::new(AtomicBool::new(true)),
            stem_gains: std::array::from_fn(|_| atomic_f32(1.0)),
            high_pass_enabled: AtomicBool::new(false),
            high_pass_hz: atomic_f32(30.0),
            low_pass_enabled: AtomicBool::new(false),
            low_pass_hz: atomic_f32(18_000.0),
            bass_gain: atomic_f32(1.0),
            mid_gain: atomic_f32(1.0),
            high_gain: atomic_f32(1.0),
            kill_bass: AtomicBool::new(false),
            kill_mid: AtomicBool::new(false),
            kill_high: AtomicBool::new(false),
            tape_delay_ms: atomic_f32(375.0),
            tape_feedback: atomic_f32(0.35),
            tape_mix: atomic_f32(0.0),
            spring_mix: atomic_f32(0.0),
            spring_decay: atomic_f32(0.78),
            auto_gain_enabled: AtomicBool::new(true),
            auto_gain_target_lufs: atomic_f32(-14.0),
        }
    }

    fn replace_sample_bank(&self, bank: Arc<SampleBank>) {
        let retired = self.sample_bank.swap(bank);
        if let Ok(mut retired_frames) = self.retired_sample_frames.lock() {
            for frames in retired.shared_frames() {
                if !retired_frames
                    .iter()
                    .any(|old_frames| Arc::ptr_eq(old_frames, &frames))
                {
                    retired_frames.push(frames);
                }
            }
            // The callback owns individual frame Arcs, not whole sample banks. Retain any
            // frames it may still use so their final deallocation happens off the audio thread.
            drop(retired);
            retired_frames.retain(|frames| Arc::strong_count(frames) > 1);
        } else {
            // On a poisoned mutex, intentionally leak the bank rather than let a callback
            // become the final owner of any sample memory.
            std::mem::forget(retired);
        }
    }

    fn collect_retired_sample_frames(&self) {
        if let Ok(mut retired_frames) = self.retired_sample_frames.lock() {
            retired_frames.retain(|frames| Arc::strong_count(frames) > 1);
        }
    }

    fn snapshot(&self) -> AudioParameters {
        let active_stems = self.active_stem_count.load(Ordering::Relaxed).clamp(2, 4) as usize;
        AudioParameters {
            master_gain: load_f32(&self.master_gain),
            stem_gains: std::array::from_fn(|index| {
                if index < active_stems {
                    load_f32(&self.stem_gains[index])
                } else {
                    0.0
                }
            }),
            high_pass_enabled: self.high_pass_enabled.load(Ordering::Relaxed),
            high_pass_hz: load_f32(&self.high_pass_hz),
            low_pass_enabled: self.low_pass_enabled.load(Ordering::Relaxed),
            low_pass_hz: load_f32(&self.low_pass_hz),
            bass_gain: load_f32(&self.bass_gain),
            mid_gain: load_f32(&self.mid_gain),
            high_gain: load_f32(&self.high_gain),
            kill_bass: self.kill_bass.load(Ordering::Relaxed),
            kill_mid: self.kill_mid.load(Ordering::Relaxed),
            kill_high: self.kill_high.load(Ordering::Relaxed),
            tape_delay_ms: load_f32(&self.tape_delay_ms),
            tape_feedback: load_f32(&self.tape_feedback),
            tape_mix: load_f32(&self.tape_mix),
            spring_mix: load_f32(&self.spring_mix),
            spring_decay: load_f32(&self.spring_decay),
            auto_gain_enabled: self.auto_gain_enabled.load(Ordering::Relaxed),
            auto_gain_target_lufs: load_f32(&self.auto_gain_target_lufs),
        }
    }

    fn apply(&self, update: AudioControlUpdate) -> Result<(), String> {
        validate_update(&update)?;
        if let Some(value) = update.master_gain {
            store_f32(&self.master_gain, value);
        }
        if let Some(value) = update.playback_rate {
            store_f32(&self.playback_rate, value);
        }
        if let Some(value) = update.pitch_lock_enabled {
            self.pitch_lock_enabled.store(value, Ordering::Relaxed);
        }
        if let Some(values) = update.stem_gains {
            for (destination, value) in self.stem_gains.iter().zip(values) {
                store_f32(destination, value);
            }
        }
        if let Some(value) = update.high_pass_enabled {
            self.high_pass_enabled.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.high_pass_hz {
            store_f32(&self.high_pass_hz, value);
        }
        if let Some(value) = update.low_pass_enabled {
            self.low_pass_enabled.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.low_pass_hz {
            store_f32(&self.low_pass_hz, value);
        }
        if let Some(value) = update.bass_gain {
            store_f32(&self.bass_gain, value);
        }
        if let Some(value) = update.mid_gain {
            store_f32(&self.mid_gain, value);
        }
        if let Some(value) = update.high_gain {
            store_f32(&self.high_gain, value);
        }
        if let Some(value) = update.kill_bass {
            self.kill_bass.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.kill_mid {
            self.kill_mid.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.kill_high {
            self.kill_high.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.tape_delay_ms {
            store_f32(&self.tape_delay_ms, value);
        }
        if let Some(value) = update.tape_feedback {
            store_f32(&self.tape_feedback, value);
        }
        if let Some(value) = update.tape_mix {
            store_f32(&self.tape_mix, value);
        }
        if let Some(value) = update.spring_mix {
            store_f32(&self.spring_mix, value);
        }
        if let Some(value) = update.spring_decay {
            store_f32(&self.spring_decay, value);
        }
        if let Some(value) = update.auto_gain_enabled {
            self.auto_gain_enabled.store(value, Ordering::Relaxed);
        }
        if let Some(value) = update.auto_gain_target_lufs {
            store_f32(&self.auto_gain_target_lufs, value);
        }
        Ok(())
    }
}

fn validate_update(update: &AudioControlUpdate) -> Result<(), String> {
    let check = |name: &str, value: f32, low: f32, high: f32| {
        if value.is_finite() && (low..=high).contains(&value) {
            Ok(())
        } else {
            Err(format!(
                "{name} must be finite and between {low} and {high}"
            ))
        }
    };
    if let Some(value) = update.master_gain {
        check("masterGain", value, 0.0, 2.0)?;
    }
    if let Some(value) = update.playback_rate {
        check("playbackRate", value, 0.8, 1.2)?;
    }
    if let Some(value) = update.auto_gain_target_lufs {
        check("autoGainTargetLufs", value, -36.0, -6.0)?;
    }
    if let Some(values) = update.stem_gains {
        for value in values {
            check("stemGains", value, 0.0, 2.0)?;
        }
    }
    for (name, value) in [
        ("bassGain", update.bass_gain),
        ("midGain", update.mid_gain),
        ("highGain", update.high_gain),
    ] {
        if let Some(value) = value {
            check(name, value, 0.0, 2.0)?;
        }
    }
    if let Some(value) = update.high_pass_hz {
        check("highPassHz", value, 10.0, 2_000.0)?;
    }
    if let Some(value) = update.low_pass_hz {
        check("lowPassHz", value, 500.0, 20_000.0)?;
    }
    if let Some(value) = update.tape_delay_ms {
        check("tapeDelayMs", value, 10.0, 2_000.0)?;
    }
    if let Some(value) = update.tape_feedback {
        check("tapeFeedback", value, 0.0, 0.92)?;
    }
    if let Some(value) = update.tape_mix {
        check("tapeMix", value, 0.0, 1.0)?;
    }
    if let Some(value) = update.spring_mix {
        check("springMix", value, 0.0, 1.0)?;
    }
    if let Some(value) = update.spring_decay {
        check("springDecay", value, 0.1, 0.97)?;
    }
    Ok(())
}

fn atomic_f32(value: f32) -> AtomicU32 {
    AtomicU32::new(value.to_bits())
}

#[inline]
fn load_f32(value: &AtomicU32) -> f32 {
    f32::from_bits(value.load(Ordering::Relaxed))
}

#[inline]
fn store_f32(destination: &AtomicU32, value: f32) {
    destination.store(value.to_bits(), Ordering::Relaxed);
}

struct EngineStatus {
    loaded: AtomicBool,
    playing: AtomicBool,
    finished: AtomicBool,
    failed: AtomicBool,
    sample_rate: AtomicU32,
    playback_rate: AtomicU32,
    position_frames: AtomicU64,
    timeline_generation: AtomicU64,
    underruns: AtomicU64,
    device_errors: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl EngineStatus {
    fn new() -> Self {
        Self {
            loaded: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            sample_rate: AtomicU32::new(0),
            playback_rate: atomic_f32(1.0),
            position_frames: AtomicU64::new(0),
            timeline_generation: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            device_errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    fn reset(&self) {
        self.loaded.store(false, Ordering::Release);
        self.playing.store(false, Ordering::Release);
        self.finished.store(false, Ordering::Release);
        self.failed.store(false, Ordering::Release);
        self.sample_rate.store(0, Ordering::Relaxed);
        store_f32(&self.playback_rate, 1.0);
        self.position_frames.store(0, Ordering::Relaxed);
        self.timeline_generation.fetch_add(1, Ordering::AcqRel);
        self.underruns.store(0, Ordering::Relaxed);
        self.device_errors.store(0, Ordering::Relaxed);
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = None;
        }
    }

    fn snapshot(&self) -> AudioStatus {
        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        AudioStatus {
            loaded: self.loaded.load(Ordering::Acquire),
            playing: self.playing.load(Ordering::Acquire),
            finished: self.finished.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            backend: backend_name(),
            sample_rate,
            active_stems: 4,
            playback_rate: load_f32(&self.playback_rate),
            limiter_latency_ms: 0.0,
            master_gain_reduction_db: 0.0,
            position_frames: self.position_frames.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            device_errors: self.device_errors.load(Ordering::Relaxed),
            last_error: self.last_error.lock().ok().and_then(|error| error.clone()),
        }
    }
}

fn backend_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "WASAPI"
    }
    #[cfg(target_os = "android")]
    {
        "AAudio"
    }
    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    {
        "CPAL default host"
    }
}

pub struct AudioEngine {
    owner: Arc<ControlThreadOwner>,
    controls: Arc<AudioControls>,
    status: Arc<EngineStatus>,
    master_bus: Arc<MasterBus>,
    deck_output: Arc<DeckOutput>,
    deck_id: usize,
}

impl Clone for AudioEngine {
    fn clone(&self) -> Self {
        Self {
            owner: Arc::clone(&self.owner),
            controls: Arc::clone(&self.controls),
            status: Arc::clone(&self.status),
            master_bus: Arc::clone(&self.master_bus),
            deck_output: Arc::clone(&self.deck_output),
            deck_id: self.deck_id,
        }
    }
}

impl AudioEngine {
    pub fn new() -> std::io::Result<Self> {
        let (first, second, recording) = Self::new_pair()?;
        drop(second);
        drop(recording);
        Ok(first)
    }

    pub(crate) fn new_pair() -> std::io::Result<(Self, Self, Arc<RecordingHub>)> {
        let recording = RecordingHub::new().map_err(std::io::Error::other)?;
        let status_a = Arc::new(EngineStatus::new());
        let status_b = Arc::new(EngineStatus::new());
        let controls_a = Arc::new(AudioControls::new());
        let controls_b = Arc::new(AudioControls::new());
        let (master_bus, outputs) = MasterBus::new(
            Arc::clone(&recording),
            [Arc::clone(&status_a), Arc::clone(&status_b)],
            [
                Arc::clone(&controls_a.pitch_lock_enabled),
                Arc::clone(&controls_b.pitch_lock_enabled),
            ],
        );
        let [output_a, output_b] = outputs;
        let first = Self::new_with_master(
            Arc::clone(&master_bus),
            Arc::new(DeckOutput::new(output_a)),
            status_a,
            controls_a,
            0,
        )?;
        let second = Self::new_with_master(
            master_bus,
            Arc::new(DeckOutput::new(output_b)),
            status_b,
            controls_b,
            1,
        )?;
        Ok((first, second, recording))
    }

    fn new_with_master(
        master_bus: Arc<MasterBus>,
        deck_output: Arc<DeckOutput>,
        status: Arc<EngineStatus>,
        controls: Arc<AudioControls>,
        deck_id: usize,
    ) -> std::io::Result<Self> {
        let (command_tx, command_rx) = mpsc::sync_channel(8);
        let thread_controls = Arc::clone(&controls);
        let thread_status = Arc::clone(&status);
        let thread_master_bus = Arc::clone(&master_bus);
        let thread_deck_output = Arc::clone(&deck_output);
        let control_thread = thread::Builder::new()
            .name(format!("championvibe-audio-control-{deck_id}"))
            .spawn(move || {
                audio_control_loop(
                    command_rx,
                    thread_controls,
                    thread_status,
                    thread_master_bus,
                    thread_deck_output,
                    deck_id,
                )
            })?;
        Ok(Self {
            owner: Arc::new(ControlThreadOwner {
                command_tx,
                thread: Mutex::new(Some(control_thread)),
            }),
            controls,
            status,
            master_bus,
            deck_output,
            deck_id,
        })
    }

    pub fn load(&self, stems: StemFiles, grid: BeatGrid) -> Result<AudioStatus, String> {
        self.load_at(stems, grid, 0)
    }

    /// Load a track with the locally persisted EBU R128 integrated-loudness measurement.
    pub fn load_with_loudness(
        &self,
        stems: StemFiles,
        grid: BeatGrid,
        integrated_lufs: Option<f64>,
    ) -> Result<AudioStatus, String> {
        self.load_at_with_loudness(stems, grid, 0, integrated_lufs)
    }

    pub fn active_stem_count(&self) -> u8 {
        self.controls
            .active_stem_count
            .load(Ordering::Acquire)
            .clamp(2, 4) as u8
    }

    pub fn set_active_stem_profile(&self, active_stems: u8) -> Result<(), String> {
        let stem_gains = match active_stems {
            2 => [1.0, 1.0, 0.0, 0.0],
            4 => [1.0, 1.0, 1.0, 1.0],
            _ => return Err("active stem profile must be 2 or 4".to_owned()),
        };
        self.update_controls(AudioControlUpdate {
            stem_gains: Some(stem_gains),
            ..AudioControlUpdate::default()
        })?;
        self.controls
            .active_stem_count
            .store(u32::from(active_stems), Ordering::Release);
        Ok(())
    }

    /// Load aligned local stems and begin decoding at a Whisper/chorus cue.
    /// The decoder skips to the cue off the audio callback, before prebuffering.
    pub fn load_at(
        &self,
        stems: StemFiles,
        grid: BeatGrid,
        start_at_ms: u64,
    ) -> Result<AudioStatus, String> {
        self.load_at_with_loudness(stems, grid, start_at_ms, None)
    }

    pub fn load_at_with_loudness(
        &self,
        stems: StemFiles,
        grid: BeatGrid,
        start_at_ms: u64,
        integrated_lufs: Option<f64>,
    ) -> Result<AudioStatus, String> {
        if start_at_ms > 86_400_000 {
            return Err("stem cue must be within the first 24 hours".to_owned());
        }
        if integrated_lufs
            .is_some_and(|value| !value.is_finite() || !(-80.0..=10.0).contains(&value))
        {
            return Err("integrated LUFS must be finite and between -80 and 10".to_owned());
        }
        let active_stems = self
            .controls
            .active_stem_count
            .load(Ordering::Acquire)
            .clamp(2, 4) as usize;
        self.request(|reply| EngineCommand::Load {
            stems,
            grid,
            start_at_ms,
            active_stems,
            integrated_lufs,
            reply,
        })
    }

    pub fn play(&self) -> Result<AudioStatus, String> {
        self.request(EngineCommand::Play)
    }

    pub fn pause(&self) -> Result<AudioStatus, String> {
        self.request(EngineCommand::Pause)
    }

    pub fn stop(&self) -> Result<AudioStatus, String> {
        self.request(EngineCommand::Stop)
    }

    pub fn rewind(&self, sample_id: Option<u32>) -> Result<AudioStatus, String> {
        let rewind_sample = if let Some(id) = sample_id {
            let bank = self.controls.sample_bank.load_full();
            let clip = bank
                .get(id)
                .ok_or_else(|| "the selected rewind sample is not loaded".to_owned())?;
            Some(SamplePlaybackClip::from_clip(clip))
        } else {
            None
        };
        self.request(|reply| EngineCommand::Rewind {
            rewind_sample,
            reply,
        })
    }

    pub fn trigger_sample(
        &self,
        sample_id: u32,
        quantization: SampleQuantization,
    ) -> Result<AudioStatus, String> {
        let sample_bank = self.controls.sample_bank.load_full();
        let clip = sample_bank
            .get(sample_id)
            .ok_or_else(|| "the selected sample is not loaded".to_owned())?;
        let clip = SamplePlaybackClip::from_clip(clip);
        self.request(|reply| EngineCommand::TriggerSample {
            clip,
            quantization,
            reply,
        })
    }

    pub(crate) fn push_tts_audio_buffer(
        &self,
        pcm: &[u8],
        sample_rate: u32,
        channels: u8,
        quantization: SampleQuantization,
    ) -> Result<TtsAudioSchedule, String> {
        if matches!(quantization, SampleQuantization::Off) {
            return Err("master TTS playback must be quantized to a beat or bar".to_owned());
        }
        if !self.status.loaded.load(Ordering::Acquire) {
            return Err("no active master deck is loaded".to_owned());
        }
        if self.status.finished.load(Ordering::Acquire) {
            return Err(
                "the active master deck has finished; restart it before scheduling TTS".to_owned(),
            );
        }
        if !self.status.playing.load(Ordering::Acquire) {
            return Err("the active master deck must be playing to quantize TTS audio".to_owned());
        }
        let frames: Arc<[StereoFrame]> = decode_tts_pcm_f32le(pcm, sample_rate, channels)?.into();
        let (reply, response) = mpsc::sync_channel(1);
        self.owner
            .command_tx
            .send(EngineCommand::QueueTtsAudio {
                frames,
                sample_rate,
                quantization,
                reply,
            })
            .map_err(|_| "native audio control thread is unavailable".to_owned())?;
        response
            .recv()
            .map_err(|_| "native audio control thread stopped before scheduling TTS".to_owned())?
    }

    pub fn set_auto_rewind(&self, enabled: bool) -> AudioStatus {
        self.controls.auto_rewind.store(enabled, Ordering::Release);
        self.status()
    }

    pub fn sample_catalog(&self) -> Vec<SampleInfo> {
        self.controls.collect_retired_sample_frames();
        self.controls.sample_bank.load().infos()
    }

    pub fn load_sample_folder(&self, path: &std::path::Path) -> Result<Vec<SampleInfo>, String> {
        let bank = SampleBank::load_folder(path)?;
        let samples = bank.infos();
        let _update_guard = self
            .controls
            .sample_bank_update
            .lock()
            .map_err(|_| "sample bank update lock is poisoned".to_owned())?;
        self.controls.replace_sample_bank(Arc::new(bank));
        self.controls.collect_retired_sample_frames();
        Ok(samples)
    }

    pub fn add_sample_file(
        &self,
        path: &std::path::Path,
        display_name: Option<&str>,
    ) -> Result<SampleInfo, String> {
        self.add_sample_file_routed(path, display_name, false)
    }

    pub(crate) fn add_tts_file(
        &self,
        path: &std::path::Path,
        display_name: Option<&str>,
    ) -> Result<SampleInfo, String> {
        self.add_sample_file_routed(path, display_name, true)
    }

    fn add_sample_file_routed(
        &self,
        path: &std::path::Path,
        display_name: Option<&str>,
        tts_aux: bool,
    ) -> Result<SampleInfo, String> {
        let _update_guard = self
            .controls
            .sample_bank_update
            .lock()
            .map_err(|_| "sample bank update lock is poisoned".to_owned())?;
        let current = self.controls.sample_bank.load_full();
        let bank = if tts_aux {
            current.with_tts_wav(path, display_name)?
        } else {
            current.with_wav(path, display_name)?
        };
        let info = bank
            .infos()
            .into_iter()
            .last()
            .ok_or_else(|| "sample bank is empty".to_owned())?;
        self.controls.replace_sample_bank(Arc::new(bank));
        self.controls.collect_retired_sample_frames();
        Ok(info)
    }

    pub fn update_controls(&self, update: AudioControlUpdate) -> Result<AudioStatus, String> {
        self.controls.apply(update)?;
        Ok(self.status())
    }

    pub fn status(&self) -> AudioStatus {
        self.controls.collect_retired_sample_frames();
        self.master_bus.collect_finished_tts_audio();
        self.with_master_status(self.status.snapshot())
    }

    fn with_master_status(&self, mut status: AudioStatus) -> AudioStatus {
        status.active_stems = self.active_stem_count();
        let meter = self.master_bus.meter();
        status.limiter_latency_ms = meter.limiter_latency_ms();
        status.master_gain_reduction_db = meter.gain_reduction_db();
        if meter.failed() {
            status.failed = true;
            if status.last_error.is_none() {
                status.last_error = meter.last_error();
            }
        }
        status.device_errors = status.device_errors.max(meter.device_errors());
        status
    }

    fn request(
        &self,
        command: impl FnOnce(SyncSender<Result<AudioStatus, String>>) -> EngineCommand,
    ) -> Result<AudioStatus, String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.owner
            .command_tx
            .send(command(reply))
            .map_err(|_| "native audio control thread is unavailable".to_owned())?;
        response
            .recv()
            .map_err(|_| "native audio control thread stopped before replying".to_owned())?
            .map(|status| self.with_master_status(status))
    }
}

struct ControlThreadOwner {
    command_tx: SyncSender<EngineCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ControlThreadOwner {
    fn drop(&mut self) {
        let _ = self.command_tx.send(EngineCommand::Shutdown);
        if let Ok(thread) = self.thread.get_mut() {
            if let Some(thread) = thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[derive(Clone)]
struct SamplePlaybackClip {
    frames: Arc<[StereoFrame]>,
    sample_rate: u32,
    tts_aux: bool,
}

impl SamplePlaybackClip {
    fn from_clip(clip: &SampleClip) -> Self {
        Self {
            frames: Arc::clone(&clip.frames),
            sample_rate: clip.sample_rate,
            tts_aux: clip.tts_aux,
        }
    }

    fn duration_frames_at(&self, output_rate: u32) -> usize {
        if output_rate == 0 || self.sample_rate == 0 {
            return 0;
        }
        ((self.frames.len() as f64 * f64::from(output_rate)) / f64::from(self.sample_rate)).ceil()
            as usize
    }
}

enum EngineCommand {
    Load {
        stems: StemFiles,
        grid: BeatGrid,
        start_at_ms: u64,
        active_stems: usize,
        integrated_lufs: Option<f64>,
        reply: SyncSender<Result<AudioStatus, String>>,
    },
    TriggerSample {
        clip: SamplePlaybackClip,
        quantization: SampleQuantization,
        reply: SyncSender<Result<AudioStatus, String>>,
    },
    QueueTtsAudio {
        frames: Arc<[StereoFrame]>,
        sample_rate: u32,
        quantization: SampleQuantization,
        reply: SyncSender<Result<TtsAudioSchedule, String>>,
    },
    Rewind {
        rewind_sample: Option<SamplePlaybackClip>,
        reply: SyncSender<Result<AudioStatus, String>>,
    },
    Play(SyncSender<Result<AudioStatus, String>>),
    Pause(SyncSender<Result<AudioStatus, String>>),
    Stop(SyncSender<Result<AudioStatus, String>>),
    Shutdown,
}

struct SampleTriggerEvent {
    clip: SamplePlaybackClip,
    start_frame: u64,
}

fn insert_sorted_sample_event(
    events: &mut [Option<SampleTriggerEvent>],
    event_count: &mut usize,
    event: SampleTriggerEvent,
) -> bool {
    if *event_count >= events.len() {
        return false;
    }
    let mut insert_at = *event_count;
    while insert_at > 0
        && events[insert_at - 1]
            .as_ref()
            .is_some_and(|previous| previous.start_frame > event.start_frame)
    {
        events[insert_at] = events[insert_at - 1].take();
        insert_at -= 1;
    }
    events[insert_at] = Some(event);
    *event_count += 1;
    true
}

#[derive(Default)]
struct ActiveSample {
    frames: Option<Arc<[StereoFrame]>>,
    sample_rate: u32,
    source_frame: f64,
    tts_aux: bool,
    active: bool,
}

#[derive(Clone, Default)]
struct QuantizationGrid {
    beat_frames: Vec<u64>,
    interval_frames: Option<u64>,
    sample_rate: u32,
}

impl QuantizationGrid {
    fn from_grid(grid: &BeatGrid, sample_rate: u32) -> Self {
        let mut beat_frames = grid
            .beat_times_seconds
            .iter()
            .copied()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0 && *seconds < 86_400.0)
            .map(|seconds| (seconds * f64::from(sample_rate)).round() as u64)
            .collect::<Vec<_>>();
        beat_frames.sort_unstable();
        beat_frames.dedup();
        beat_frames.truncate(250_000);

        let interval_frames = if beat_frames.len() > 1 {
            let mut intervals = beat_frames
                .windows(2)
                .map(|pair| pair[1].saturating_sub(pair[0]))
                .filter(|interval| *interval > 0)
                .collect::<Vec<_>>();
            intervals.sort_unstable();
            intervals.get(intervals.len() / 2).copied()
        } else {
            grid.bpm
                .filter(|bpm| bpm.is_finite() && (20.0..=400.0).contains(bpm))
                .map(|bpm| (f64::from(sample_rate) * 60.0 / bpm).round().max(1.0) as u64)
        };
        Self {
            beat_frames,
            interval_frames,
            sample_rate,
        }
    }

    fn next_frame(&self, current_frame: u64, quantization: SampleQuantization) -> u64 {
        match quantization {
            SampleQuantization::Off => current_frame,
            SampleQuantization::Beat | SampleQuantization::Bar => {
                let bar_quantized = matches!(quantization, SampleQuantization::Bar);
                let mut beat_index = self
                    .beat_frames
                    .partition_point(|frame| *frame < current_frame);
                if bar_quantized {
                    while beat_index % 4 != 0 {
                        beat_index = beat_index.saturating_add(1);
                    }
                }
                if let Some(frame) = self.beat_frames.get(beat_index) {
                    return *frame;
                }
                let interval = self.interval_frames.unwrap_or(0);
                if interval == 0 {
                    return current_frame;
                }
                if let Some(last) = self.beat_frames.last().copied() {
                    let next_index = beat_index.max(self.beat_frames.len());
                    let steps = next_index
                        .saturating_sub(self.beat_frames.len())
                        .saturating_add(1) as u64;
                    last.saturating_add(interval.saturating_mul(steps.max(1)))
                } else {
                    let mut step = current_frame / interval;
                    if step.saturating_mul(interval) < current_frame {
                        step = step.saturating_add(1);
                    }
                    if bar_quantized {
                        let remainder = step % 4;
                        if remainder != 0 {
                            step = step.saturating_add(4 - remainder);
                        }
                    }
                    step.saturating_mul(interval)
                }
            }
        }
    }
}

fn next_tts_grid_frame(
    grid: &QuantizationGrid,
    current_frame: u64,
    quantization: SampleQuantization,
) -> Result<u64, String> {
    if matches!(quantization, SampleQuantization::Off) {
        return Err("master TTS playback must be quantized to a beat or bar".to_owned());
    }
    if grid.beat_frames.is_empty() && grid.interval_frames.is_none() {
        return Err("the active master deck has no usable beat grid for TTS scheduling".to_owned());
    }
    // Allow a short control/IPC lead. If the next grid point is too close to enqueue safely, use
    // the following beat/bar rather than starting a few frames late.
    let scheduling_lead_frames = (u64::from(grid.sample_rate) * 30 / 1_000).max(1);
    Ok(grid.next_frame(
        current_frame.saturating_add(scheduling_lead_frames),
        quantization,
    ))
}

struct PlaybackSession {
    renderer: JoinHandle<Producer<DeckFrame>>,
    stems: StemFiles,
    grid: BeatGrid,
    active_stems: usize,
    integrated_lufs: Option<f64>,
    quantization_grid: QuantizationGrid,
    sample_events: Producer<SampleTriggerEvent>,
    cancel_workers: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    paused_at_end: bool,
}

fn audio_control_loop(
    receiver: Receiver<EngineCommand>,
    controls: Arc<AudioControls>,
    status: Arc<EngineStatus>,
    master_bus: Arc<MasterBus>,
    deck_output: Arc<DeckOutput>,
    deck_id: usize,
) {
    let mut session: Option<PlaybackSession> = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(40)) {
            Ok(EngineCommand::Load {
                stems,
                grid,
                start_at_ms,
                active_stems,
                integrated_lufs,
                reply,
            }) => {
                if let Some(old) = session.take() {
                    dispose_session(old, &controls, &deck_output);
                }
                controls.playing.store(false, Ordering::Release);
                master_bus.flush_deck(deck_id);
                status.reset();
                match create_session(
                    stems,
                    grid,
                    start_at_ms,
                    Arc::clone(&controls),
                    Arc::clone(&status),
                    integrated_lufs,
                    false,
                    None,
                    active_stems,
                    &master_bus,
                    &deck_output,
                    deck_id,
                ) {
                    Ok(new_session) => {
                        status.loaded.store(true, Ordering::Release);
                        session = Some(new_session);
                        let _ = reply.send(Ok(status.snapshot()));
                    }
                    Err(error) => {
                        status.failed.store(true, Ordering::Release);
                        set_last_error(&status, error.clone());
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Ok(EngineCommand::TriggerSample {
                clip,
                quantization,
                reply,
            }) => {
                let result = match session.as_mut() {
                    Some(_) if status.finished.load(Ordering::Acquire) => {
                        Err("track has finished; use Wheel and Come Again before triggering more samples".to_owned())
                    }
                    Some(session) => {
                        let current_frame = status.position_frames.load(Ordering::Relaxed);
                        let start_frame = session
                            .quantization_grid
                            .next_frame(current_frame, quantization);
                        session
                            .sample_events
                            .push(SampleTriggerEvent { clip, start_frame })
                            .map(|_| status.snapshot())
                            .map_err(|_| "sample trigger queue is full".to_owned())
                    }
                    None => Err("no stem set is loaded".to_owned()),
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::QueueTtsAudio {
                frames,
                sample_rate,
                quantization,
                reply,
            }) => {
                let result = match session.as_ref() {
                    None => Err("no active master deck is loaded".to_owned()),
                    Some(_) if status.finished.load(Ordering::Acquire) => Err(
                        "the active master deck has finished; restart it before scheduling TTS"
                            .to_owned(),
                    ),
                    Some(_) if !status.playing.load(Ordering::Acquire) => Err(
                        "the active master deck must be playing to quantize TTS audio".to_owned(),
                    ),
                    Some(session) => {
                        let current_frame = status.position_frames.load(Ordering::Acquire);
                        let result = next_tts_grid_frame(
                            &session.quantization_grid,
                            current_frame,
                            quantization,
                        )
                        .and_then(|target_deck_frame| {
                            let delay_frames =
                                target_deck_frame.saturating_sub(current_frame).max(1);
                            let deck_generation =
                                status.timeline_generation.load(Ordering::Acquire);
                            master_bus
                                .schedule_tts_audio(
                                    frames,
                                    sample_rate,
                                    deck_id,
                                    deck_generation,
                                    target_deck_frame,
                                )
                                .map(|output_sample_rate| TtsAudioSchedule {
                                    target_deck_frame,
                                    delay_frames,
                                    output_sample_rate,
                                    quantization: match quantization {
                                        SampleQuantization::Beat => "beat".to_owned(),
                                        SampleQuantization::Bar => "bar".to_owned(),
                                        SampleQuantization::Off => "off".to_owned(),
                                    },
                                })
                        });
                        result
                    }
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Rewind {
                rewind_sample,
                reply,
            }) => {
                let result = rewind_session(
                    &mut session,
                    &controls,
                    &status,
                    &master_bus,
                    &deck_output,
                    deck_id,
                    rewind_sample,
                );
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Play(reply)) => {
                let result = match session.as_mut() {
                    Some(session) if !status.finished.load(Ordering::Acquire) => {
                        session.paused_at_end = false;
                        controls.playing.store(true, Ordering::Release);
                        status.playing.store(true, Ordering::Release);
                        Ok(status.snapshot())
                    }
                    Some(_) => {
                        Err("track has finished; use Wheel and Come Again to restart".to_owned())
                    }
                    None => Err("no stem set is loaded".to_owned()),
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Pause(reply)) => {
                let result = match session.as_mut() {
                    Some(_) => {
                        controls.playing.store(false, Ordering::Release);
                        status.playing.store(false, Ordering::Release);
                        Ok(status.snapshot())
                    }
                    None => Err("no stem set is loaded".to_owned()),
                };
                let _ = reply.send(result);
            }
            Ok(EngineCommand::Stop(reply)) => {
                controls.playing.store(false, Ordering::Release);
                if let Some(old) = session.take() {
                    dispose_session(old, &controls, &deck_output);
                }
                master_bus.flush_deck(deck_id);
                status.reset();
                let _ = reply.send(Ok(status.snapshot()));
            }
            Ok(EngineCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let finished = status.finished.load(Ordering::Acquire);
        let failed = status.failed.load(Ordering::Acquire);
        let should_auto_rewind = session.as_ref().is_some_and(|session| {
            finished
                && !failed
                && !session.paused_at_end
                && controls.auto_rewind.load(Ordering::Acquire)
        });
        if should_auto_rewind {
            if let Err(error) = rewind_session(
                &mut session,
                &controls,
                &status,
                &master_bus,
                &deck_output,
                deck_id,
                None,
            ) {
                controls.auto_rewind.store(false, Ordering::Release);
                status.failed.store(true, Ordering::Release);
                status.playing.store(false, Ordering::Release);
                set_last_error(&status, error);
            }
        } else if let Some(session) = session.as_mut() {
            if (finished || failed) && !session.paused_at_end {
                controls.playing.store(false, Ordering::Release);
                status.playing.store(false, Ordering::Release);
                master_bus.flush_deck(deck_id);
                if failed {
                    status.finished.store(true, Ordering::Release);
                    if let Ok(mut last_error) = status.last_error.lock() {
                        if last_error.is_none() {
                            *last_error =
                                Some("native audio output reported a device error".to_owned());
                        }
                    }
                }
                session.paused_at_end = true;
            }
        }
    }

    controls.playing.store(false, Ordering::Release);
    if let Some(old) = session.take() {
        dispose_session(old, &controls, &deck_output);
    }
    master_bus.flush_deck(deck_id);
    status.reset();
}

fn rewind_session(
    session: &mut Option<PlaybackSession>,
    controls: &Arc<AudioControls>,
    status: &Arc<EngineStatus>,
    master_bus: &MasterBus,
    deck_output: &DeckOutput,
    deck_id: usize,
    rewind_sample: Option<SamplePlaybackClip>,
) -> Result<AudioStatus, String> {
    let old = session
        .take()
        .ok_or_else(|| "no stem set is loaded".to_owned())?;
    let stems = old.stems.clone();
    let grid = old.grid.clone();
    let active_stems = old.active_stems;
    let integrated_lufs = old.integrated_lufs;
    controls.playing.store(false, Ordering::Release);
    dispose_session(old, controls, deck_output);
    master_bus.flush_deck(deck_id);

    status.loaded.store(false, Ordering::Release);
    status.playing.store(false, Ordering::Release);
    status.finished.store(false, Ordering::Release);
    status.failed.store(false, Ordering::Release);
    status.timeline_generation.fetch_add(1, Ordering::AcqRel);
    status.position_frames.store(0, Ordering::Relaxed);
    if let Ok(mut error) = status.last_error.lock() {
        *error = None;
    }

    let new_session = match create_session(
        stems,
        grid,
        0,
        Arc::clone(controls),
        Arc::clone(status),
        integrated_lufs,
        true,
        rewind_sample,
        active_stems,
        master_bus,
        deck_output,
        deck_id,
    ) {
        Ok(session) => session,
        Err(error) => {
            status.failed.store(true, Ordering::Release);
            set_last_error(status, error.clone());
            return Err(error);
        }
    };
    *session = Some(new_session);
    status.loaded.store(true, Ordering::Release);
    controls.playing.store(true, Ordering::Release);
    status.playing.store(true, Ordering::Release);
    Ok(status.snapshot())
}

fn set_last_error(status: &EngineStatus, message: String) {
    if let Ok(mut last_error) = status.last_error.lock() {
        *last_error = Some(message);
    }
}

fn create_session(
    stems: StemFiles,
    grid: BeatGrid,
    start_at_ms: u64,
    controls: Arc<AudioControls>,
    status: Arc<EngineStatus>,
    integrated_lufs: Option<f64>,
    start_with_rewind: bool,
    rewind_sample: Option<SamplePlaybackClip>,
    active_stems: usize,
    master_bus: &MasterBus,
    deck_output: &DeckOutput,
    deck_id: usize,
) -> Result<PlaybackSession, String> {
    if !matches!(active_stems, 2 | 4) {
        return Err("active stem count must be 2 or 4".to_owned());
    }
    let paths = stems.clone().into_ordered_paths();
    for path in paths.iter().take(active_stems) {
        if !path.is_file() {
            return Err(format!("stem file does not exist: {}", path.display()));
        }
    }

    // The app-wide MasterBus owns the only native CPAL stream. Each deck renders into its
    // bounded SPSC input ring and is mixed with the other deck on that shared device clock.
    let sample_rate = master_bus.sample_rate()?;
    let playback_rate = load_f32(&controls.playback_rate).clamp(0.8, 1.2);
    // Decode every stem at the device rate; WSOLA and varispeed branches are rendered from one
    // cached decoder stream and selected at the shared master output on every audio frame.
    // `position_frames` and the quantizer remain on the sped-up/slowed-down track timeline.
    let timeline_rate = (f64::from(sample_rate) / f64::from(playback_rate))
        .round()
        .clamp(8_000.0, 384_000.0) as u32;
    status.sample_rate.store(sample_rate, Ordering::Relaxed);
    store_f32(&status.playback_rate, playback_rate);
    let source_start_frame = (start_at_ms as f64 * f64::from(sample_rate) / 1_000.0)
        .round()
        .min(u64::MAX as f64) as u64;
    let start_frame = (start_at_ms as f64 * f64::from(timeline_rate) / 1_000.0)
        .round()
        .min(u64::MAX as f64) as u64;
    status.position_frames.store(start_frame, Ordering::Relaxed);

    let mut rewind_frames_remaining = 0usize;
    let (mut sample_event_tx, sample_event_rx) = RingBuffer::new(SAMPLE_EVENT_CAPACITY);
    if start_with_rewind {
        let builtin_bank = controls.sample_bank.load_full();
        let rewind_id = builtin_bank
            .id_by_name("vinyl-rewind")
            .ok_or_else(|| "the built-in vinyl rewind effect is unavailable".to_owned())?;
        let rewind_clip = builtin_bank
            .get(rewind_id)
            .ok_or_else(|| "the built-in vinyl rewind effect is unavailable".to_owned())?;
        let rewind_clip = SamplePlaybackClip::from_clip(rewind_clip);
        rewind_frames_remaining = rewind_clip
            .duration_frames_at(sample_rate)
            .max((f64::from(sample_rate) * REWIND_DURATION_SECONDS) as usize);
        sample_event_tx
            .push(SampleTriggerEvent {
                clip: rewind_clip,
                start_frame: 0,
            })
            .map_err(|_| "could not schedule the built-in rewind effect".to_owned())?;
        if let Some(rewind_sample) = rewind_sample {
            rewind_frames_remaining =
                rewind_frames_remaining.max(rewind_sample.duration_frames_at(sample_rate));
            sample_event_tx
                .push(SampleTriggerEvent {
                    clip: rewind_sample,
                    start_frame: 0,
                })
                .map_err(|_| "could not schedule the rewind sample".to_owned())?;
        }
    }
    let quantization_grid = QuantizationGrid::from_grid(&grid, timeline_rate);

    let cancel_workers = Arc::new(AtomicBool::new(false));
    let wsola_schedule = Arc::new(wsola::SharedSchedule::new());
    let (startup_tx, startup_rx) = mpsc::channel();
    let mut consumers = Vec::with_capacity(STEM_COUNT);
    let mut finished_flags = Vec::with_capacity(STEM_COUNT);
    let mut workers = Vec::with_capacity(STEM_COUNT);

    for (stem_index, path) in paths.into_iter().enumerate() {
        let capacity = if stem_index < active_stems {
            RING_CAPACITY_FRAMES
        } else {
            1
        };
        let (producer, consumer) = RingBuffer::<PitchFrame>::new(capacity);
        let finished = Arc::new(AtomicBool::new(stem_index >= active_stems));
        if stem_index >= active_stems {
            drop(producer);
            consumers.push(consumer);
            finished_flags.push(finished);
            continue;
        }
        let worker_cancel = Arc::clone(&cancel_workers);
        let worker_finished = Arc::clone(&finished);
        let worker_status = Arc::clone(&status);
        let worker_startup = startup_tx.clone();
        let worker_path = path.clone();
        let worker_schedule = Arc::clone(&wsola_schedule);
        let worker = match thread::Builder::new()
            .name(format!("championvibe-stem-{stem_index}"))
            .spawn(move || {
                decoder::run_worker(
                    worker_path,
                    sample_rate,
                    source_start_frame,
                    playback_rate,
                    stem_index,
                    worker_schedule,
                    PREBUFFER_FRAMES,
                    producer,
                    worker_cancel,
                    worker_finished,
                    worker_startup,
                    worker_status,
                )
            }) {
            Ok(worker) => worker,
            Err(error) => {
                cancel_workers.store(true, Ordering::Release);
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(format!("could not start stem decoder thread: {error}"));
            }
        };
        consumers.push(consumer);
        finished_flags.push(finished);
        workers.push(worker);
    }
    drop(startup_tx);

    for _ in 0..active_stems {
        match startup_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                cancel_workers.store(true, Ordering::Release);
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(error);
            }
            Err(error) => {
                cancel_workers.store(true, Ordering::Release);
                for worker in workers {
                    let _ = worker.join();
                }
                return Err(format!("stem prebuffer timed out: {error}"));
            }
        }
    }

    let consumers: [Consumer<PitchFrame>; STEM_COUNT] = consumers
        .try_into()
        .map_err(|_| "internal error: incorrect number of stem consumers".to_owned())?;
    let finished_flags: [Arc<AtomicBool>; STEM_COUNT] = finished_flags
        .try_into()
        .map_err(|_| "internal error: incorrect number of stem completion flags".to_owned())?;
    let graph = AudioGraph {
        consumers,
        finished: finished_flags,
        active_stems,
        controls,
        status: Arc::clone(&status),
        sample_events: sample_event_rx,
        pending_sample_events: std::array::from_fn(|_| None),
        pending_sample_event_count: 0,
        active_samples: std::array::from_fn(|_| ActiveSample::default()),
        rewind_frames_remaining,
        sample_rate,
        timeline_position_frames: start_frame,
        integrated_lufs: integrated_lufs.map(|value| value as f32),
        auto_gain: AutoGainRamp::new(sample_rate),
        dsp_wsola: DspChain::new(sample_rate),
        dsp_varispeed: DspChain::new(sample_rate),
        dsp_samples: DspChain::new(sample_rate),
        dsp_tts: DspChain::new(sample_rate),
    };

    let producer_slot = Arc::new(Mutex::new(Some(deck_output.take_producer()?)));
    let thread_producer_slot = Arc::clone(&producer_slot);
    let thread_cancel = Arc::clone(&cancel_workers);
    let renderer = match thread::Builder::new()
        .name(format!("championvibe-deck-render-{deck_id}"))
        .spawn(move || {
            let producer = thread_producer_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("a deck renderer takes its output producer exactly once");
            run_render_worker(graph, producer, thread_cancel)
        }) {
        Ok(renderer) => renderer,
        Err(error) => {
            cancel_workers.store(true, Ordering::Release);
            for worker in workers {
                let _ = worker.join();
            }
            if let Ok(mut producer) = producer_slot.lock() {
                if let Some(producer) = producer.take() {
                    deck_output.return_producer(producer);
                }
            }
            return Err(format!("could not start deck render thread: {error}"));
        }
    };

    Ok(PlaybackSession {
        renderer,
        stems,
        grid,
        active_stems,
        integrated_lufs,
        quantization_grid,
        sample_events: sample_event_tx,
        cancel_workers,
        workers,
        paused_at_end: false,
    })
}

fn run_render_worker(
    mut graph: AudioGraph,
    mut producer: Producer<DeckFrame>,
    cancel: Arc<AtomicBool>,
) -> Producer<DeckFrame> {
    let mut pending_frame = None;
    while !cancel.load(Ordering::Acquire) {
        if !graph.controls.playing.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
            continue;
        }

        let mut frame = pending_frame.take().unwrap_or_else(|| graph.render_frame());
        loop {
            if cancel.load(Ordering::Acquire) {
                return producer;
            }
            if !graph.controls.playing.load(Ordering::Acquire) {
                pending_frame = Some(frame);
                break;
            }
            match producer.push(frame) {
                Ok(()) => break,
                Err(PushError::Full(returned)) => {
                    frame = returned;
                    thread::sleep(Duration::from_micros(250));
                }
            }
        }
    }
    producer
}

struct AudioGraph {
    consumers: [Consumer<PitchFrame>; STEM_COUNT],
    finished: [Arc<AtomicBool>; STEM_COUNT],
    active_stems: usize,
    controls: Arc<AudioControls>,
    status: Arc<EngineStatus>,
    sample_events: Consumer<SampleTriggerEvent>,
    pending_sample_events: [Option<SampleTriggerEvent>; PENDING_SAMPLE_EVENT_CAPACITY],
    pending_sample_event_count: usize,
    active_samples: [ActiveSample; MAX_ACTIVE_SAMPLES],
    rewind_frames_remaining: usize,
    sample_rate: u32,
    timeline_position_frames: u64,
    integrated_lufs: Option<f32>,
    auto_gain: AutoGainRamp,
    dsp_wsola: DspChain,
    dsp_varispeed: DspChain,
    dsp_samples: DspChain,
    dsp_tts: DspChain,
}

impl AudioGraph {
    #[inline]
    fn render_frame(&mut self) -> DeckFrame {
        let parameters = self.controls.snapshot();
        self.dsp_wsola.prepare(&parameters);
        self.dsp_varispeed.prepare(&parameters);
        self.dsp_samples.prepare(&parameters);
        self.dsp_tts.prepare(&parameters);
        let track_frame = self.timeline_position_frames;
        let (mut mixed, sample_mix, tts_mix, advanced, underrun) =
            self.next_frame(&parameters, track_frame);
        let gain_db = AutoGainRamp::target_db(
            parameters.auto_gain_enabled,
            self.integrated_lufs,
            parameters.auto_gain_target_lufs,
        );
        let auto_gain = self.auto_gain.next_gain(gain_db);
        mixed.wsola.left *= auto_gain;
        mixed.wsola.right *= auto_gain;
        mixed.varispeed.left *= auto_gain;
        mixed.varispeed.right *= auto_gain;
        let wsola_audio = self.dsp_wsola.process(mixed.wsola, &parameters);
        let varispeed_audio = self.dsp_varispeed.process(mixed.varispeed, &parameters);
        let sample_audio = self.dsp_samples.process(sample_mix, &parameters);
        let tts_audio = self.dsp_tts.process(tts_mix, &parameters);
        if advanced {
            self.timeline_position_frames = self.timeline_position_frames.saturating_add(1);
        }
        if underrun {
            self.status.underruns.fetch_add(1, Ordering::Relaxed);
        }
        DeckFrame {
            wsola_audio: StereoFrame {
                left: finite_sample(wsola_audio.left),
                right: finite_sample(wsola_audio.right),
            },
            varispeed_audio: StereoFrame {
                left: finite_sample(varispeed_audio.left),
                right: finite_sample(varispeed_audio.right),
            },
            sample_audio: StereoFrame {
                left: finite_sample(sample_audio.left),
                right: finite_sample(sample_audio.right),
            },
            tts_audio: StereoFrame {
                left: finite_sample(tts_audio.left),
                right: finite_sample(tts_audio.right),
            },
            advances_timeline: advanced,
        }
    }

    #[inline]
    fn next_frame(
        &mut self,
        parameters: &AudioParameters,
        track_frame: u64,
    ) -> (PitchFrame, StereoFrame, StereoFrame, bool, bool) {
        if !self.controls.playing.load(Ordering::Relaxed) {
            return (
                PitchFrame::default(),
                StereoFrame::default(),
                StereoFrame::default(),
                false,
                false,
            );
        }
        if self.status.failed.load(Ordering::Acquire) {
            self.controls.playing.store(false, Ordering::Release);
            self.status.playing.store(false, Ordering::Release);
            self.status.finished.store(true, Ordering::Release);
            return (
                PitchFrame::default(),
                StereoFrame::default(),
                StereoFrame::default(),
                false,
                false,
            );
        }

        let rewinding = self.rewind_frames_remaining > 0;
        let stems_finished = (0..self.active_stems).all(|index| {
            self.finished[index].load(Ordering::Acquire) && self.consumers[index].is_empty()
        });
        if !rewinding && !stems_finished {
            for index in 0..self.active_stems {
                if self.consumers[index].is_empty() && !self.finished[index].load(Ordering::Acquire)
                {
                    return (
                        PitchFrame::default(),
                        StereoFrame::default(),
                        StereoFrame::default(),
                        false,
                        true,
                    );
                }
            }
        }

        self.start_due_samples(track_frame, stems_finished && !rewinding);
        if stems_finished
            && self.active_samples.iter().all(|sample| !sample.active)
            && self.pending_sample_event_count == 0
            && self.sample_events.is_empty()
        {
            self.controls.playing.store(false, Ordering::Release);
            self.status.playing.store(false, Ordering::Release);
            self.status.finished.store(true, Ordering::Release);
            return (
                PitchFrame::default(),
                StereoFrame::default(),
                StereoFrame::default(),
                false,
                false,
            );
        }

        let mut wsola_sum = StereoFrame::default();
        let mut varispeed_sum = StereoFrame::default();
        let mut advanced = false;
        if rewinding {
            self.rewind_frames_remaining = self.rewind_frames_remaining.saturating_sub(1);
        } else if !stems_finished {
            for index in 0..self.active_stems {
                if let Ok(stem) = self.consumers[index].pop() {
                    let gain = parameters.stem_gains[index];
                    wsola_sum.left += stem.wsola.left * gain;
                    wsola_sum.right += stem.wsola.right * gain;
                    varispeed_sum.left += stem.varispeed.left * gain;
                    varispeed_sum.right += stem.varispeed.right * gain;
                }
            }
            advanced = true;
        }

        // Samples and TTS bypass the WSOLA/varispeed crossfade and remain distinct master inputs.
        let mut sample_mix = StereoFrame::default();
        let mut tts_mix = StereoFrame::default();
        self.mix_active_samples(&mut sample_mix, &mut tts_mix);
        (
            PitchFrame {
                wsola: wsola_sum,
                varispeed: varispeed_sum,
            },
            sample_mix,
            tts_mix,
            advanced,
            false,
        )
    }

    #[inline]
    fn start_due_samples(&mut self, track_frame: u64, force_due: bool) {
        while self.pending_sample_event_count < PENDING_SAMPLE_EVENT_CAPACITY {
            let Ok(event) = self.sample_events.pop() else {
                break;
            };
            insert_sorted_sample_event(
                &mut self.pending_sample_events,
                &mut self.pending_sample_event_count,
                event,
            );
        }

        while self.pending_sample_event_count > 0 {
            let Some(event) = self.pending_sample_events[0].as_ref() else {
                self.remove_pending_sample_event(0);
                continue;
            };
            if !force_due && event.start_frame > track_frame {
                break;
            }
            let Some(slot_index) = self.active_samples.iter().position(|sample| !sample.active)
            else {
                break;
            };
            let Some(event) = self.pending_sample_events[0].take() else {
                self.remove_pending_sample_event(0);
                continue;
            };
            self.active_samples[slot_index] = ActiveSample {
                frames: Some(event.clip.frames),
                sample_rate: event.clip.sample_rate,
                source_frame: 0.0,
                tts_aux: event.clip.tts_aux,
                active: true,
            };
            self.remove_pending_sample_event(0);
        }
    }

    fn remove_pending_sample_event(&mut self, index: usize) {
        if index >= self.pending_sample_event_count {
            return;
        }
        for position in index + 1..self.pending_sample_event_count {
            self.pending_sample_events[position - 1] = self.pending_sample_events[position].take();
        }
        self.pending_sample_event_count -= 1;
        self.pending_sample_events[self.pending_sample_event_count] = None;
    }

    #[inline]
    fn mix_active_samples(&mut self, sample_bus: &mut StereoFrame, tts_bus: &mut StereoFrame) {
        for active in &mut self.active_samples {
            if !active.active {
                continue;
            }
            let Some(frames) = active.frames.as_ref() else {
                active.active = false;
                continue;
            };
            let frame_index = active.source_frame.floor() as usize;
            let Some(first) = frames.get(frame_index).copied() else {
                active.active = false;
                active.frames = None;
                continue;
            };
            let second = frames
                .get(frame_index.saturating_add(1))
                .copied()
                .unwrap_or(first);
            let fraction = (active.source_frame - frame_index as f64) as f32;
            let bus: &mut StereoFrame = if active.tts_aux {
                &mut *tts_bus
            } else {
                &mut *sample_bus
            };
            bus.left += first.left + (second.left - first.left) * fraction;
            bus.right += first.right + (second.right - first.right) * fraction;
            active.source_frame +=
                f64::from(active.sample_rate) / f64::from(self.sample_rate.max(1));
            if active.source_frame >= frames.len() as f64 {
                active.active = false;
                active.frames = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_tts_pcm_decodes_little_endian_mono_and_stereo_frames() {
        let mut mono = Vec::new();
        mono.extend_from_slice(&0.25_f32.to_le_bytes());
        mono.extend_from_slice(&(-0.5_f32).to_le_bytes());
        let decoded = decode_tts_pcm_f32le(&mono, 24_000, 1).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].left, 0.25);
        assert_eq!(decoded[0].right, 0.25);
        assert_eq!(decoded[1].left, -0.5);
        assert_eq!(decoded[1].right, -0.5);

        let mut stereo = Vec::new();
        stereo.extend_from_slice(&0.75_f32.to_le_bytes());
        stereo.extend_from_slice(&(-0.125_f32).to_le_bytes());
        let decoded = decode_tts_pcm_f32le(&stereo, 48_000, 2).unwrap();
        assert_eq!(
            decoded,
            vec![StereoFrame {
                left: 0.75,
                right: -0.125
            }]
        );
    }

    #[test]
    fn binary_tts_pcm_rejects_json_arrays_bad_alignment_and_unsafe_samples() {
        assert!(decode_tts_pcm_f32le(b"[0.1,0.2]", 24_000, 1).is_err());
        assert!(decode_tts_pcm_f32le(&[0, 1, 2], 24_000, 1).is_err());
        assert!(decode_tts_pcm_f32le(&f32::NAN.to_le_bytes(), 24_000, 1).is_err());
        assert!(decode_tts_pcm_f32le(&9.0_f32.to_le_bytes(), 24_000, 1).is_err());
        assert!(decode_tts_pcm_f32le(&0.0_f32.to_le_bytes(), 7_999, 1).is_err());
        assert!(decode_tts_pcm_f32le(&0.0_f32.to_le_bytes(), 24_000, 3).is_err());
    }

    #[test]
    fn tts_scheduler_selects_a_strictly_future_beat_or_bar_on_the_current_grid() {
        let grid = QuantizationGrid::from_grid(
            &BeatGrid {
                beat_times_seconds: vec![0.0, 0.5, 1.0, 1.5, 2.0],
                bpm: Some(120.0),
            },
            100,
        );
        assert_eq!(
            next_tts_grid_frame(&grid, 25, SampleQuantization::Beat).unwrap(),
            50
        );
        assert_eq!(
            next_tts_grid_frame(&grid, 50, SampleQuantization::Beat).unwrap(),
            100
        );
        assert_eq!(
            next_tts_grid_frame(&grid, 25, SampleQuantization::Bar).unwrap(),
            200
        );
        assert_eq!(
            next_tts_grid_frame(&grid, 50, SampleQuantization::Bar).unwrap(),
            200
        );
        assert!(next_tts_grid_frame(&grid, 25, SampleQuantization::Off).is_err());
        assert!(
            next_tts_grid_frame(&QuantizationGrid::default(), 25, SampleQuantization::Beat)
                .is_err()
        );

        let bpm_only = QuantizationGrid::from_grid(
            &BeatGrid {
                beat_times_seconds: Vec::new(),
                bpm: Some(120.0),
            },
            100,
        );
        assert_eq!(
            next_tts_grid_frame(&bpm_only, 25, SampleQuantization::Beat).unwrap(),
            50
        );
    }

    #[test]
    fn two_stem_profile_keeps_inactive_gains_muted_after_deck_reset() {
        let controls = AudioControls::new();
        controls.active_stem_count.store(2, Ordering::Release);
        controls
            .apply(AudioControlUpdate {
                stem_gains: Some([1.0; STEM_COUNT]),
                ..AudioControlUpdate::default()
            })
            .unwrap();
        assert_eq!(controls.snapshot().stem_gains, [1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn adjustable_auto_gain_target_and_pitch_lock_controls_are_validated() {
        let controls = AudioControls::new();
        controls
            .apply(AudioControlUpdate {
                auto_gain_enabled: Some(true),
                auto_gain_target_lufs: Some(-12.0),
                playback_rate: Some(1.15),
                pitch_lock_enabled: Some(false),
                ..AudioControlUpdate::default()
            })
            .unwrap();
        let parameters = controls.snapshot();
        assert!(parameters.auto_gain_enabled);
        assert_eq!(parameters.auto_gain_target_lufs, -12.0);
        assert!(!controls.pitch_lock_enabled.load(Ordering::Relaxed));
        assert!(validate_update(&AudioControlUpdate {
            playback_rate: Some(1.21),
            ..AudioControlUpdate::default()
        })
        .is_err());
        assert!(validate_update(&AudioControlUpdate {
            auto_gain_target_lufs: Some(-40.0),
            ..AudioControlUpdate::default()
        })
        .is_err());
    }

    #[test]
    fn pending_sample_events_are_sorted_by_target_frame() {
        let mut events: [Option<SampleTriggerEvent>; 4] = std::array::from_fn(|_| None);
        let mut event_count = 0;
        let sample_bank = SampleBank::builtins();
        let sample_id = sample_bank.id_by_name("siren").unwrap();
        let Some(source_clip) = sample_bank.get(sample_id) else {
            panic!("built-in siren sample was not found");
        };
        let clip = SamplePlaybackClip::from_clip(source_clip);
        for start_frame in [120, 40, 80] {
            assert!(insert_sorted_sample_event(
                &mut events,
                &mut event_count,
                SampleTriggerEvent {
                    clip: clip.clone(),
                    start_frame,
                },
            ));
        }
        let frames = events[..event_count]
            .iter()
            .flatten()
            .map(|event| event.start_frame)
            .collect::<Vec<_>>();
        assert_eq!(frames, vec![40, 80, 120]);
    }

    #[test]
    fn retired_sample_frames_wait_for_queued_trigger_references() {
        let controls = AudioControls::new();
        let bank = controls.sample_bank.load_full();
        let sample_id = bank.id_by_name("siren").unwrap();
        let Some(source_clip) = bank.get(sample_id) else {
            panic!("built-in siren sample was not found");
        };
        let queued_trigger_clip = SamplePlaybackClip::from_clip(source_clip);
        drop(bank);

        controls.replace_sample_bank(Arc::new(SampleBank::builtins()));
        controls.collect_retired_sample_frames();
        let retained_count = controls
            .retired_sample_frames
            .lock()
            .map(|frames| frames.len())
            .unwrap_or_default();
        assert_eq!(retained_count, 1);

        drop(queued_trigger_clip);
        controls.collect_retired_sample_frames();
        let retired_empty = controls
            .retired_sample_frames
            .lock()
            .map(|frames| frames.is_empty())
            .unwrap_or(false);
        assert!(retired_empty);
    }

    #[test]
    fn sample_quantization_uses_beat_and_bar_positions() {
        let grid = BeatGrid {
            beat_times_seconds: (0..8).map(|beat| f64::from(beat) * 0.5).collect(),
            bpm: Some(120.0),
        };
        let quantization = QuantizationGrid::from_grid(&grid, 48_000);
        assert_eq!(quantization.next_frame(100, SampleQuantization::Off), 100);
        assert_eq!(
            quantization.next_frame(100, SampleQuantization::Beat),
            24_000
        );
        assert_eq!(
            quantization.next_frame(24_000, SampleQuantization::Beat),
            24_000
        );
        assert_eq!(
            quantization.next_frame(25_000, SampleQuantization::Bar),
            96_000
        );
    }

    #[test]
    fn sample_quantization_falls_back_to_bpm_when_beat_times_are_missing() {
        let grid = BeatGrid {
            beat_times_seconds: Vec::new(),
            bpm: Some(120.0),
        };
        let quantization = QuantizationGrid::from_grid(&grid, 48_000);
        assert_eq!(
            quantization.next_frame(24_100, SampleQuantization::Beat),
            48_000
        );
        assert_eq!(
            quantization.next_frame(24_100, SampleQuantization::Bar),
            96_000
        );
    }
}

fn dispose_session(session: PlaybackSession, controls: &AudioControls, deck_output: &DeckOutput) {
    controls.playing.store(false, Ordering::Release);
    let PlaybackSession {
        renderer,
        cancel_workers,
        workers,
        ..
    } = session;
    cancel_workers.store(true, Ordering::Release);
    for worker in workers {
        let _ = worker.join();
    }
    if let Ok(producer) = renderer.join() {
        deck_output.return_producer(producer);
    }
}
