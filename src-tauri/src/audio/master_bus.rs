//! Shared hardware output and post-summation master processing.
//!
//! Deck renderers produce stereo frames into bounded SPSC rings. This module owns the one CPAL
//! output stream, combines both decks with separate sample and TTS auxiliary buses on the device
//! clock, applies the live master limiter, and taps the post-limiter signal for recording. The
//! CPAL data callback performs no disk I/O, heap allocation, or mutex locking.

use super::cpal_stream::{
    BufferRecoveryPolicy, CallbackWatchdog, GlitchKind, GlitchSnapshot, GlitchSource,
    GlitchTelemetry, NOMINAL_BUFFER_FRAMES,
};
use super::{
    limiter::LookaheadLimiter, wsola::PitchLockTransition, CaptureClock, CapturedFrame, DeckFrame,
    EngineStatus, RecordingHub, StereoFrame,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, ErrorKind, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedBufferSize,
};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
    Arc, Mutex, TryLockError,
};

const MASTER_DECK_RING_FRAMES: usize = 2_048;
const MASTER_TTS_EVENT_CAPACITY: usize = 16;
const PENDING_MASTER_TTS_EVENTS: usize = MASTER_TTS_EVENT_CAPACITY * 2;
const MAX_ACTIVE_MASTER_TTS: usize = 4;
const MAX_MASTER_TTS_BUFFERS: usize = 4;
pub(super) const MAX_MASTER_TTS_BYTES: usize = super::MAX_TTS_PCM_BYTES;
const DB_METER_SCALE: f32 = 100.0;

#[derive(Clone, Copy, Debug)]
pub(super) struct AudioOutputConfig {
    pub sample_rate: u32,
}

/// Atomic telemetry shared with the Tauri status command. All data-callback writes are lock-free.
pub(super) struct MasterMeter {
    gain_reduction_db_x100: AtomicU32,
    limiter_latency_us: AtomicU32,
    device_errors: AtomicU64,
    failed: AtomicU8,
    last_error: Mutex<Option<String>>,
}

impl MasterMeter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gain_reduction_db_x100: AtomicU32::new(0),
            limiter_latency_us: AtomicU32::new(0),
            device_errors: AtomicU64::new(0),
            failed: AtomicU8::new(0),
            last_error: Mutex::new(None),
        })
    }

    pub(super) fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db_x100.load(Ordering::Relaxed) as f32 / DB_METER_SCALE
    }

    pub(super) fn limiter_latency_ms(&self) -> f32 {
        self.limiter_latency_us.load(Ordering::Relaxed) as f32 / 1_000.0
    }

    pub(super) fn device_errors(&self) -> u64 {
        self.device_errors.load(Ordering::Relaxed)
    }

    pub(super) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire) != 0
    }

    pub(super) fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|value| value.clone())
    }
}

/// Pure frame-level Deck A/B + sample + TTS master summation, limiter, and digital-rail guard.
///
/// The limiter's finite lookahead and oversampled true-peak estimate are the primary protection.
/// The final sample clamp guarantees representable PCM samples cannot exceed full scale. It does
/// not claim to make every downstream DAC/driver intersample peak physically impossible.
pub(super) struct MasterMixer {
    limiter: LookaheadLimiter,
    meter: Arc<MasterMeter>,
}

impl MasterMixer {
    pub(super) fn new(sample_rate: u32, meter: Arc<MasterMeter>) -> Self {
        meter.limiter_latency_us.store(
            (LookaheadLimiter::latency_ms_for_sample_rate(sample_rate) * 1_000.0)
                .round()
                .max(0.0) as u32,
            Ordering::Release,
        );
        Self {
            limiter: LookaheadLimiter::new(sample_rate),
            meter,
        }
    }

    #[inline]
    pub(super) fn process_frame(
        &mut self,
        deck_a: StereoFrame,
        deck_b: StereoFrame,
        samples: StereoFrame,
        tts: StereoFrame,
    ) -> StereoFrame {
        let sum = StereoFrame {
            left: finite_sample(deck_a.left)
                + finite_sample(deck_b.left)
                + finite_sample(samples.left)
                + finite_sample(tts.left),
            right: finite_sample(deck_a.right)
                + finite_sample(deck_b.right)
                + finite_sample(samples.right)
                + finite_sample(tts.right),
        };
        let (limited, gain_reduction_db) = self.limiter.process_with_reduction(sum);
        self.meter.gain_reduction_db_x100.store(
            (gain_reduction_db.max(0.0) * DB_METER_SCALE)
                .round()
                .clamp(0.0, u32::MAX as f32) as u32,
            Ordering::Relaxed,
        );
        StereoFrame {
            left: finite_sample(limited.left).clamp(-1.0, 1.0),
            right: finite_sample(limited.right).clamp(-1.0, 1.0),
        }
    }
}

struct MasterTtsEvent {
    frames: Arc<[StereoFrame]>,
    sample_rate: u32,
    deck_id: usize,
    deck_generation: u64,
    start_deck_frame: u64,
}

#[derive(Default)]
struct ActiveMasterTts {
    frames: Option<Arc<[StereoFrame]>>,
    sample_rate: u32,
    source_frame: f64,
}

/// Fixed-capacity, callback-owned TTS auxiliary reader. PCM buffers are prepared off the audio
/// thread, then resampled into the device-rate master sum without locks or allocation.
struct MasterTtsAux {
    events: Consumer<MasterTtsEvent>,
    pending: [Option<MasterTtsEvent>; PENDING_MASTER_TTS_EVENTS],
    pending_count: usize,
    active: [ActiveMasterTts; MAX_ACTIVE_MASTER_TTS],
    output_sample_rate: u32,
}

impl MasterTtsAux {
    fn new(events: Consumer<MasterTtsEvent>, output_sample_rate: u32) -> Self {
        Self {
            events,
            pending: std::array::from_fn(|_| None),
            pending_count: 0,
            active: std::array::from_fn(|_| ActiveMasterTts::default()),
            output_sample_rate: output_sample_rate.max(1),
        }
    }

    #[inline]
    fn next_frame(
        &mut self,
        deck_positions: [u64; 2],
        deck_generations: [u64; 2],
        deck_playing: [bool; 2],
        deck_finished: [bool; 2],
    ) -> StereoFrame {
        while self.pending_count < self.pending.len() {
            let Ok(event) = self.events.pop() else {
                break;
            };
            if self.pending_count >= self.pending.len() {
                break;
            }
            self.pending[self.pending_count] = Some(event);
            self.pending_count += 1;
        }

        let mut pending_index = 0;
        while pending_index < self.pending_count {
            let Some((deck_id, deck_generation, start_deck_frame)) = self.pending[pending_index]
                .as_ref()
                .map(|event| (event.deck_id, event.deck_generation, event.start_deck_frame))
            else {
                self.remove_pending(pending_index);
                continue;
            };
            if deck_id >= deck_positions.len()
                || deck_generations[deck_id] != deck_generation
                || deck_finished[deck_id]
            {
                self.remove_pending(pending_index);
                continue;
            }
            if !deck_playing[deck_id] || deck_positions[deck_id] < start_deck_frame {
                pending_index += 1;
                continue;
            }
            let Some(slot_index) = self.active.iter().position(|clip| clip.frames.is_none()) else {
                break;
            };
            let Some(event) = self.pending[pending_index].take() else {
                self.remove_pending(pending_index);
                continue;
            };
            self.active[slot_index] = ActiveMasterTts {
                frames: Some(event.frames),
                sample_rate: event.sample_rate,
                source_frame: 0.0,
            };
            self.remove_pending(pending_index);
        }

        let mut sum = StereoFrame::default();
        for clip in &mut self.active {
            let Some(frames) = clip.frames.as_ref() else {
                continue;
            };
            let frame_index = clip.source_frame.floor() as usize;
            let Some(first) = frames.get(frame_index).copied() else {
                clip.frames = None;
                continue;
            };
            let second = frames
                .get(frame_index.saturating_add(1))
                .copied()
                .unwrap_or(first);
            let fraction = (clip.source_frame - frame_index as f64) as f32;
            sum.left += first.left + (second.left - first.left) * fraction;
            sum.right += first.right + (second.right - first.right) * fraction;
            clip.source_frame += f64::from(clip.sample_rate) / f64::from(self.output_sample_rate);
            if clip.source_frame >= frames.len() as f64 {
                clip.frames = None;
            }
        }
        sum
    }

    fn remove_pending(&mut self, index: usize) {
        if index >= self.pending_count {
            return;
        }
        for position in index + 1..self.pending_count {
            self.pending[position - 1] = self.pending[position].take();
        }
        self.pending_count -= 1;
        self.pending[self.pending_count] = None;
    }
}

/// App-wide audio output. A bus opens at most one CPAL stream and keeps it for the app lifetime.
pub(super) struct DeckOutput {
    producer: Mutex<Option<Producer<DeckFrame>>>,
}

impl DeckOutput {
    pub(super) fn new(producer: Producer<DeckFrame>) -> Self {
        Self {
            producer: Mutex::new(Some(producer)),
        }
    }

    pub(super) fn take_producer(&self) -> Result<Producer<DeckFrame>, String> {
        self.producer
            .lock()
            .map_err(|_| "deck output producer lock is poisoned".to_owned())?
            .take()
            .ok_or_else(|| "deck output producer is already active".to_owned())
    }

    pub(super) fn return_producer(&self, producer: Producer<DeckFrame>) {
        if let Ok(mut slot) = self.producer.lock() {
            *slot = Some(producer);
        }
    }
}

pub(super) struct MasterBus {
    recording: Arc<RecordingHub>,
    meter: Arc<MasterMeter>,
    statuses: [Arc<EngineStatus>; 2],
    pitch_lock_enabled: [Arc<AtomicBool>; 2],
    flush_requested: [Arc<AtomicBool>; 2],
    tts_producer: Mutex<Producer<MasterTtsEvent>>,
    retained_tts_buffers: Mutex<Vec<Arc<[StereoFrame]>>>,
    glitches: Arc<GlitchTelemetry>,
    recovery: Mutex<BufferRecoveryPolicy>,
    state: Mutex<MasterBusState>,
}

struct MasterBusState {
    stream: Option<Stream>,
    /// Everything needed to rebuild the same stream with a larger hardware buffer.
    setup: Option<StreamSetup>,
    config: Option<AudioOutputConfig>,
    deck_consumers: Option<[Consumer<DeckFrame>; 2]>,
    recording_producer: Option<Producer<CapturedFrame>>,
    recording_consumer: Option<Consumer<CapturedFrame>>,
    tts_consumer: Option<Consumer<MasterTtsEvent>>,
}

struct StreamSetup {
    device: cpal::Device,
    device_name: String,
    sample_format: SampleFormat,
    config: StreamConfig,
    graph: Arc<Mutex<MasterOutputGraph>>,
}

impl MasterBus {
    pub(super) fn new(
        recording: Arc<RecordingHub>,
        statuses: [Arc<EngineStatus>; 2],
        pitch_lock_enabled: [Arc<AtomicBool>; 2],
    ) -> (Arc<Self>, [Producer<DeckFrame>; 2]) {
        let (deck_a_producer, deck_a_consumer) = RingBuffer::new(MASTER_DECK_RING_FRAMES);
        let (deck_b_producer, deck_b_consumer) = RingBuffer::new(MASTER_DECK_RING_FRAMES);
        let (recording_producer, recording_consumer) = recording.new_capture_ring();
        let (tts_producer, tts_consumer) = RingBuffer::new(MASTER_TTS_EVENT_CAPACITY);
        let glitches = GlitchTelemetry::new(GlitchSource::detect(), NOMINAL_BUFFER_FRAMES);
        let bus = Arc::new(Self {
            recording,
            meter: MasterMeter::new(),
            statuses,
            pitch_lock_enabled,
            recovery: Mutex::new(BufferRecoveryPolicy::new(None, None)),
            glitches,
            tts_producer: Mutex::new(tts_producer),
            retained_tts_buffers: Mutex::new(Vec::new()),
            flush_requested: [
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicBool::new(true)),
            ],
            state: Mutex::new(MasterBusState {
                stream: None,
                setup: None,
                config: None,
                deck_consumers: Some([deck_a_consumer, deck_b_consumer]),
                recording_producer: Some(recording_producer),
                recording_consumer: Some(recording_consumer),
                tts_consumer: Some(tts_consumer),
            }),
        });
        (bus, [deck_a_producer, deck_b_producer])
    }

    /// Internal glitch telemetry for the status command.
    pub(super) fn glitch_snapshot(&self) -> GlitchSnapshot {
        self.drain_glitch_telemetry();
        self.glitches.snapshot()
    }

    /// Turns callback-side counters into log lines and grows the hardware buffer when the driver
    /// keeps glitching. Cheap enough to sit on the status path: it only inspects atomics while no
    /// glitch is pending.
    fn drain_glitch_telemetry(&self) {
        let glitches = self.glitches.drain_recoverable_glitches();
        let now_ms = self.glitches.now_ms();
        let plan = {
            let mut policy = match self.recovery.lock() {
                Ok(policy) => policy,
                Err(_) => return,
            };
            policy.note_observed_frames(self.glitches.observed_callback_frames());
            policy.plan_growth(glitches, now_ms)
        };
        let Some(frames) = plan else {
            if glitches > 0 && self.recovery.lock().is_ok_and(|policy| policy.exhausted()) {
                self.glitches.note_buffer_exhausted();
            }
            return;
        };
        let outcome = self.rebuild_output_stream(frames);
        let succeeded = outcome.is_ok();
        if let Ok(mut policy) = self.recovery.lock() {
            policy.note_growth(succeeded);
        }
        match outcome {
            Ok(()) => self.glitches.note_buffer_frames(frames),
            // A failed rebuild is a device-level problem, not an exhausted device.
            Err(error) => self.glitches.log_event(GlitchKind::DeviceError, error),
        }
    }

    /// Rebuilds the single output stream with a larger hardware buffer around the existing render
    /// graph. The deck rings, the limiter state and the recording tap are untouched, so playback
    /// resumes on the new buffer with at most one buffer of silence in between.
    fn rebuild_output_stream(&self, frames: u32) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "master audio bus state lock is poisoned".to_owned())?;
        let (device, device_name, sample_format, base_config, graph) = match state.setup.as_ref() {
            Some(setup) => (
                setup.device.clone(),
                setup.device_name.clone(),
                setup.sample_format,
                setup.config,
                Arc::clone(&setup.graph),
            ),
            None => return Err("no native output stream is open to rebuild".to_owned()),
        };
        let config = StreamConfig {
            buffer_size: BufferSize::Fixed(frames),
            ..base_config
        };
        if let Some(stream) = state.stream.as_ref() {
            // Stop pulling frames from the graph before a second stream would consume them too.
            let _ = stream.pause();
        }
        let rebuilt = build_stream_for_format(
            sample_format,
            &device,
            &config,
            usize::from(config.channels),
            config.sample_rate,
            Arc::clone(&graph),
            Arc::clone(&self.meter),
            self.statuses.clone(),
            Arc::clone(&self.glitches),
        );
        let rebuilt = match rebuilt {
            Ok(stream) => match stream.play() {
                Ok(()) => Ok(stream),
                Err(error) => Err(format!(
                    "could not start the rebuilt master output stream: {error}"
                )),
            },
            Err(error) => Err(error),
        };
        match rebuilt {
            Ok(stream) => {
                let previous = state.stream.replace(stream);
                drop(previous);
                // A rebuild introduces a deliberate gap that is not a glitch.
                if let Ok(mut graph) = graph.lock() {
                    graph.watchdog.reset();
                }
                self.glitches.log_event(
                    GlitchKind::BufferGrown,
                    format!("device="{device_name}" bufferFrames={frames}"),
                );
                Ok(())
            }
            Err(error) => {
                if let Some(stream) = state.stream.as_ref() {
                    let _ = stream.play();
                }
                Err(format!(
                    "could not rebuild the master output with a {frames}-frame buffer: {error}"
                ))
            }
        }
    }

    pub(super) fn meter(&self) -> Arc<MasterMeter> {
        Arc::clone(&self.meter)
    }

    pub(super) fn flush_deck(&self, deck_id: usize) {
        if let Some(requested) = self.flush_requested.get(deck_id) {
            requested.store(true, Ordering::Release);
        }
    }

    pub(super) fn sample_rate(&self) -> Result<u32, String> {
        self.ensure_started().map(|config| config.sample_rate)
    }

    /// Enqueue a preallocated PCM clip against the active deck's own beat-grid frame clock.
    pub(super) fn schedule_tts_audio(
        &self,
        frames: Arc<[StereoFrame]>,
        input_sample_rate: u32,
        deck_id: usize,
        deck_generation: u64,
        start_deck_frame: u64,
    ) -> Result<u32, String> {
        if frames.is_empty() || input_sample_rate == 0 || deck_id >= self.statuses.len() {
            return Err(
                "TTS auxiliary audio must have frames, a valid sample rate, and a valid deck"
                    .to_owned(),
            );
        }
        let config = self.ensure_started()?;
        let frame_bytes = frames
            .len()
            .saturating_mul(std::mem::size_of::<StereoFrame>());
        if frame_bytes > MAX_MASTER_TTS_BYTES {
            return Err("TTS audio buffer exceeds the 64 MiB master-aux limit".to_owned());
        }
        let mut producer = self
            .tts_producer
            .lock()
            .map_err(|_| "master TTS queue lock is poisoned".to_owned())?;
        let mut retained = self
            .retained_tts_buffers
            .lock()
            .map_err(|_| "master TTS buffer registry lock is poisoned".to_owned())?;
        retained.retain(|buffer| Arc::strong_count(buffer) > 1);
        let retained_bytes = retained
            .iter()
            .map(|buffer| {
                buffer
                    .len()
                    .saturating_mul(std::mem::size_of::<StereoFrame>())
            })
            .fold(0usize, usize::saturating_add);
        if retained.len() >= MAX_MASTER_TTS_BUFFERS
            || retained_bytes.saturating_add(frame_bytes) > MAX_MASTER_TTS_BYTES
        {
            return Err(
                "master TTS queue is busy; wait for the current shoutout to finish".to_owned(),
            );
        }

        let event = MasterTtsEvent {
            frames: Arc::clone(&frames),
            sample_rate: input_sample_rate,
            deck_id,
            deck_generation,
            start_deck_frame,
        };
        retained.push(frames);
        match producer.push(event) {
            Ok(()) => Ok(config.sample_rate),
            Err(PushError::Full(event)) => {
                drop(event);
                retained.pop();
                Err("master TTS event queue is full".to_owned())
            }
        }
    }

    /// Frees completed clips away from the CPAL callback after its final Arc has been released and
    /// lets the buffer recovery act on the glitches the callback recorded.
    pub(super) fn collect_finished_tts_audio(&self) {
        if let Ok(mut retained) = self.retained_tts_buffers.lock() {
            retained.retain(|buffer| Arc::strong_count(buffer) > 1);
        }
        self.drain_glitch_telemetry();
    }

    fn ensure_started(&self) -> Result<AudioOutputConfig, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "master audio bus state lock is poisoned".to_owned())?;
        if let Some(config) = state.config {
            return Ok(config);
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default audio output device is available".to_owned())?;
        let device_name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_else(|_| "unknown output device".to_owned());
        let supported = device
            .default_output_config()
            .map_err(|error| format!("could not query output device format: {error}"))?;
        let sample_format = supported.sample_format();
        let (default_buffer_frames, max_buffer_frames) = buffer_size_range(supported.buffer_size());
        let config: StreamConfig = supported.into();
        let channels = config.channels as usize;
        let sample_rate = config.sample_rate;
        if channels == 0 || sample_rate == 0 {
            return Err("audio device reported an invalid stream format".to_owned());
        }

        let consumers = state
            .deck_consumers
            .take()
            .ok_or_else(|| "master output ring buffers were already consumed".to_owned())?;
        let capture_producer = state
            .recording_producer
            .take()
            .ok_or_else(|| "master recording tap was already consumed".to_owned())?;
        let capture_consumer = state
            .recording_consumer
            .take()
            .ok_or_else(|| "master recording reader was already consumed".to_owned())?;
        let tts_consumer = state
            .tts_consumer
            .take()
            .ok_or_else(|| "master TTS aux reader was already consumed".to_owned())?;
        self.recording.attach(capture_consumer, sample_rate)?;

        let [deck_a, deck_b] = consumers;
        let clock = Arc::clone(&self.recording.clock);
        let graph = Arc::new(Mutex::new(MasterOutputGraph {
            mixer: MasterMixer::new(sample_rate, Arc::clone(&self.meter)),
            deck_a,
            deck_b,
            tts_aux: MasterTtsAux::new(tts_consumer, sample_rate),
            recording_producer: capture_producer,
            recording: Arc::clone(&self.recording),
            clock,
            pitch_transitions: std::array::from_fn(|index| {
                PitchLockTransition::new(
                    sample_rate,
                    self.pitch_lock_enabled[index].load(Ordering::Acquire),
                )
            }),
            statuses: self.statuses.clone(),
            pitch_lock_enabled: self.pitch_lock_enabled.clone(),
            flush_requested: self.flush_requested.clone(),
            sample_rate,
            watchdog: CallbackWatchdog::new(),
            glitches: Arc::clone(&self.glitches),
        }));
        if let Ok(mut policy) = self.recovery.lock() {
            *policy = BufferRecoveryPolicy::new(default_buffer_frames, max_buffer_frames);
            self.glitches.note_buffer_frames(policy.frames());
        }
        let stream = build_stream_for_format(
            sample_format,
            &device,
            &config,
            channels,
            sample_rate,
            Arc::clone(&graph),
            Arc::clone(&self.meter),
            self.statuses.clone(),
            Arc::clone(&self.glitches),
        )?;
        stream
            .play()
            .map_err(|error| format!("could not start the native master output stream: {error}"))?;

        let output = AudioOutputConfig { sample_rate };
        state.config = Some(output);
        state.setup = Some(StreamSetup {
            device,
            device_name,
            sample_format,
            config,
            graph,
        });
        state.stream = Some(stream);
        Ok(output)
    }
}

struct MasterOutputGraph {
    mixer: MasterMixer,
    deck_a: Consumer<DeckFrame>,
    deck_b: Consumer<DeckFrame>,
    tts_aux: MasterTtsAux,
    recording_producer: Producer<CapturedFrame>,
    recording: Arc<RecordingHub>,
    clock: Arc<CaptureClock>,
    statuses: [Arc<EngineStatus>; 2],
    pitch_lock_enabled: [Arc<AtomicBool>; 2],
    pitch_transitions: [PitchLockTransition; 2],
    flush_requested: [Arc<AtomicBool>; 2],
    sample_rate: u32,
    watchdog: CallbackWatchdog,
    glitches: Arc<GlitchTelemetry>,
}

/// Buffer sizes the device advertises. Both are optional because some hosts, notably AAudio,
/// report `SupportedBufferSize::Unknown`.
fn buffer_size_range(supported: &SupportedBufferSize) -> (Option<u32>, Option<u32>) {
    match supported {
        SupportedBufferSize::Range { min, max } => (Some(*min), Some(*max)),
        SupportedBufferSize::Unknown => (None, None),
    }
}

/// Builds one output stream for whichever sample format the device wants.
fn build_stream_for_format(
    sample_format: SampleFormat,
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    sample_rate: u32,
    graph: Arc<Mutex<MasterOutputGraph>>,
    meter: Arc<MasterMeter>,
    statuses: [Arc<EngineStatus>; 2],
    glitches: Arc<GlitchTelemetry>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::F32 => build_stream::<f32>(
            device,
            config,
            channels,
            sample_rate,
            graph,
            meter,
            statuses,
            glitches,
        ),
        SampleFormat::I16 => build_stream::<i16>(
            device,
            config,
            channels,
            sample_rate,
            graph,
            meter,
            statuses,
            glitches,
        ),
        SampleFormat::U16 => build_stream::<u16>(
            device,
            config,
            channels,
            sample_rate,
            graph,
            meter,
            statuses,
            glitches,
        ),
        format => Err(format!(
            "unsupported native output sample format: {format:?}"
        )),
    }
}

/// Overwrites a callback buffer with silence. Used when the render graph is unavailable, so a
/// rebuild can never leave uninitialised samples on the device.
fn silent_output<T: FromSample<f32>>(output: &mut [T]) {
    for sample in output.iter_mut() {
        *sample = T::from_sample_(0.0_f32);
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    sample_rate: u32,
    graph: Arc<Mutex<MasterOutputGraph>>,
    meter: Arc<MasterMeter>,
    statuses: [Arc<EngineStatus>; 2],
    glitches: Arc<GlitchTelemetry>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let callback_meter = Arc::clone(&meter);
    let error_statuses = statuses.clone();
    let callback_glitches = Arc::clone(&glitches);
    let error_glitches = Arc::clone(&glitches);
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |output: &mut [T], info| match graph.try_lock() {
                Ok(mut graph) => graph.render(output, channels, info),
                Err(TryLockError::Poisoned(_)) => {
                    callback_glitches.record(GlitchKind::DeviceError);
                    silent_output(output);
                }
                Err(TryLockError::WouldBlock) => {
                    // Only a buffer rebuild holds the graph; the frames it is not rendering during
                    // this callback are silence, and the glitch is visible in the telemetry.
                    callback_glitches.record(GlitchKind::LockContention);
                    silent_output(output);
                }
            },
            move |error| {
                let kind = error.kind();
                // Android's AAudio backend reports every xrun as `ErrorKind::Xrun`, and a xrun is
                // recoverable: it must not raise the permanent device-failure flag.
                let recoverable = matches!(kind, ErrorKind::Xrun | ErrorKind::RealtimeDenied);
                if matches!(kind, ErrorKind::Xrun) {
                    error_glitches.record(GlitchKind::Xrun);
                } else {
                    error_glitches.record(GlitchKind::DeviceError);
                }
                callback_meter.device_errors.fetch_add(1, Ordering::Relaxed);
                if !recoverable {
                    callback_meter.failed.store(1, Ordering::Release);
                }
                let detail = format!("native master audio device error ({kind:?}): {error}");
                if let Ok(mut last_error) = callback_meter.last_error.lock() {
                    *last_error = Some(detail);
                }
                for status in &error_statuses {
                    status.device_errors.fetch_add(1, Ordering::Relaxed);
                    if !recoverable {
                        status.failed.store(true, Ordering::Release);
                    }
                }
            },
            None,
        )
        .map_err(|error| {
            format!("could not create the single native master output stream: {error}")
        })
}

impl MasterOutputGraph {
    fn render<T: FromSample<f32>>(
        &mut self,
        output: &mut [T],
        channels: usize,
        info: &cpal::OutputCallbackInfo,
    ) {
        if channels == 0 {
            return;
        }
        let callback_ns = self.clock.now_ns();
        let frames = (output.len() / channels) as u32;
        self.glitches.observe_callback_frames(frames);
        if let Some(glitch) = self.watchdog.observe(callback_ns, frames, self.sample_rate) {
            self.glitches.record(glitch);
        }
        let timestamp = info.timestamp();
        let playback_delay = timestamp.playback.duration_since(timestamp.callback);
        let callback_start_ns = self
            .clock
            .now_ns()
            .saturating_add(playback_delay.as_nanos().min(u64::MAX as u128) as u64);
        let capture_this_callback = self.recording.flags.is_active();
        let active_a = self.statuses[0].playing.load(Ordering::Relaxed);
        let active_b = self.statuses[1].playing.load(Ordering::Relaxed);
        let discard_a = self.flush_requested[0].swap(false, Ordering::AcqRel);
        let discard_b = self.flush_requested[1].swap(false, Ordering::AcqRel);
        if discard_a {
            while self.deck_a.pop().is_ok() {}
        }
        if discard_b {
            while self.deck_b.pop().is_ok() {}
        }
        let mut deck_a_underrun = false;
        let mut deck_b_underrun = false;

        for (frame_index, output_frame) in output.chunks_mut(channels).enumerate() {
            let deck_a = if discard_a || !active_a {
                DeckFrame::default()
            } else {
                match self.deck_a.pop() {
                    Ok(frame) => frame,
                    Err(_) => {
                        deck_a_underrun |= active_a;
                        DeckFrame::default()
                    }
                }
            };
            let deck_b = if discard_b || !active_b {
                DeckFrame::default()
            } else {
                match self.deck_b.pop() {
                    Ok(frame) => frame,
                    Err(_) => {
                        deck_b_underrun |= active_b;
                        DeckFrame::default()
                    }
                }
            };
            if deck_a.advances_timeline {
                self.statuses[0]
                    .position_frames
                    .fetch_add(1, Ordering::Relaxed);
            }
            if deck_b.advances_timeline {
                self.statuses[1]
                    .position_frames
                    .fetch_add(1, Ordering::Relaxed);
            }
            let (deck_a_audio, samples_a, tts_a) = if active_a && !discard_a {
                (
                    self.pitch_transitions[0].mix(
                        deck_a.wsola_audio,
                        deck_a.varispeed_audio,
                        self.pitch_lock_enabled[0].load(Ordering::Relaxed),
                    ),
                    deck_a.sample_audio,
                    deck_a.tts_audio,
                )
            } else {
                (
                    StereoFrame::default(),
                    StereoFrame::default(),
                    StereoFrame::default(),
                )
            };
            let (deck_b_audio, samples_b, tts_b) = if active_b && !discard_b {
                (
                    self.pitch_transitions[1].mix(
                        deck_b.wsola_audio,
                        deck_b.varispeed_audio,
                        self.pitch_lock_enabled[1].load(Ordering::Relaxed),
                    ),
                    deck_b.sample_audio,
                    deck_b.tts_audio,
                )
            } else {
                (
                    StereoFrame::default(),
                    StereoFrame::default(),
                    StereoFrame::default(),
                )
            };
            let samples = StereoFrame {
                left: samples_a.left + samples_b.left,
                right: samples_a.right + samples_b.right,
            };
            let deck_positions = [
                self.statuses[0].position_frames.load(Ordering::Relaxed),
                self.statuses[1].position_frames.load(Ordering::Relaxed),
            ];
            let deck_generations = [
                self.statuses[0].timeline_generation.load(Ordering::Relaxed),
                self.statuses[1].timeline_generation.load(Ordering::Relaxed),
            ];
            let global_tts = self.tts_aux.next_frame(
                deck_positions,
                deck_generations,
                [active_a && !discard_a, active_b && !discard_b],
                [
                    self.statuses[0].finished.load(Ordering::Relaxed),
                    self.statuses[1].finished.load(Ordering::Relaxed),
                ],
            );
            let tts = StereoFrame {
                left: tts_a.left + tts_b.left + global_tts.left,
                right: tts_a.right + tts_b.right + global_tts.right,
            };
            let mixed = self
                .mixer
                .process_frame(deck_a_audio, deck_b_audio, samples, tts);

            let callback_offset_ns = ((frame_index as u128 * 1_000_000_000_u128)
                / self.sample_rate.max(1) as u128)
                .min(u64::MAX as u128) as u64;
            let timestamp_ns = callback_start_ns.saturating_add(callback_offset_ns);
            if capture_this_callback && self.recording.flags.should_capture(timestamp_ns) {
                let frame = CapturedFrame {
                    timestamp_ns,
                    left: mixed.left,
                    right: mixed.right,
                };
                if self.recording_producer.push(frame).is_err() {
                    self.recording.flags.record_drop();
                }
            }

            if output_frame.is_empty() {
                continue;
            }
            if output_frame.len() == 1 {
                output_frame[0] = T::from_sample_((mixed.left + mixed.right) * 0.5);
            } else {
                output_frame[0] = T::from_sample_(mixed.left);
                output_frame[1] = T::from_sample_(mixed.right);
                for sample in output_frame.iter_mut().skip(2) {
                    *sample = T::from_sample_(0.0);
                }
            }
        }

        if deck_a_underrun {
            self.statuses[0].underruns.fetch_add(1, Ordering::Relaxed);
        }
        if deck_b_underrun {
            self.statuses[1].underruns.fetch_add(1, Ordering::Relaxed);
        }
        if deck_a_underrun || deck_b_underrun {
            // One starved callback is one glitch, however many frames it swallowed.
            self.glitches.record(GlitchKind::Underrun);
        }
    }
}

#[inline]
fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{MasterMeter, MasterMixer, MasterTtsAux, MasterTtsEvent};
    use crate::audio::StereoFrame;
    use rtrb::RingBuffer;
    use std::sync::Arc;

    #[test]
    fn standalone_master_tts_aux_waits_for_its_target_and_resamples_24khz_to_48khz() {
        let (mut producer, consumer) = RingBuffer::new(4);
        let frames: Arc<[StereoFrame]> = vec![
            StereoFrame {
                left: 0.0,
                right: 0.0,
            },
            StereoFrame {
                left: 1.0,
                right: -1.0,
            },
            StereoFrame {
                left: 0.0,
                right: 0.0,
            },
        ]
        .into();
        assert!(producer
            .push(MasterTtsEvent {
                frames,
                sample_rate: 24_000,
                deck_id: 0,
                deck_generation: 7,
                start_deck_frame: 2,
            })
            .is_ok());
        let mut aux = MasterTtsAux::new(consumer, 48_000);
        let generations = [7, 0];

        assert_eq!(
            aux.next_frame([0, 0], generations, [true, false], [false, false]),
            StereoFrame::default()
        );
        assert_eq!(
            aux.next_frame([1, 0], generations, [true, false], [false, false]),
            StereoFrame::default()
        );
        assert_eq!(
            aux.next_frame([2, 0], generations, [true, false], [false, false]),
            StereoFrame {
                left: 0.0,
                right: 0.0
            },
        );
        let interpolated = aux.next_frame([3, 0], generations, [true, false], [false, false]);
        assert!((interpolated.left - 0.5).abs() < 1e-6);
        assert!((interpolated.right + 0.5).abs() < 1e-6);
        assert_eq!(
            aux.next_frame([4, 0], generations, [true, false], [false, false]),
            StereoFrame {
                left: 1.0,
                right: -1.0
            },
        );
        let down_slope = aux.next_frame([5, 0], generations, [true, false], [false, false]);
        assert!((down_slope.left - 0.5).abs() < 1e-6);
        assert!((down_slope.right + 0.5).abs() < 1e-6);
    }

    #[test]
    fn master_tts_aux_discards_events_from_a_replaced_deck_timeline() {
        let (mut producer, consumer) = RingBuffer::new(4);
        let frames: Arc<[StereoFrame]> = vec![StereoFrame {
            left: 0.8,
            right: -0.8,
        }]
        .into();
        assert!(producer
            .push(MasterTtsEvent {
                frames,
                sample_rate: 48_000,
                deck_id: 0,
                deck_generation: 4,
                start_deck_frame: 0,
            })
            .is_ok());
        let mut aux = MasterTtsAux::new(consumer, 48_000);
        assert_eq!(
            aux.next_frame([0, 0], [5, 0], [true, false], [false, false]),
            StereoFrame::default(),
        );
        assert_eq!(
            aux.next_frame([0, 0], [4, 0], [true, false], [false, false]),
            StereoFrame::default(),
        );
    }

    #[test]
    fn standalone_master_tts_is_summed_before_the_master_limiter() {
        let (mut producer, consumer) = RingBuffer::new(4);
        let frames: Arc<[StereoFrame]> = vec![
            StereoFrame {
                left: 0.75,
                right: -0.75
            };
            512
        ]
        .into();
        assert!(producer
            .push(MasterTtsEvent {
                frames,
                sample_rate: 48_000,
                deck_id: 0,
                deck_generation: 1,
                start_deck_frame: 0,
            })
            .is_ok());
        let mut aux = MasterTtsAux::new(consumer, 48_000);
        let mut master = MasterMixer::new(48_000, MasterMeter::new());
        let mut peak = 0.0_f32;
        for deck_frame in 0..1_024 {
            let global_tts = aux.next_frame([deck_frame, 0], [1, 0], [true, false], [false, false]);
            let output = master.process_frame(
                StereoFrame {
                    left: 0.75,
                    right: -0.75,
                },
                StereoFrame::default(),
                StereoFrame::default(),
                global_tts,
            );
            peak = peak.max(output.left.abs()).max(output.right.abs());
        }
        assert!(
            peak > 0.75,
            "global TTS was not included in the pre-limiter sum: {peak}"
        );
        assert!(peak <= 1.0, "post-sum limiter exceeded full scale: {peak}");
    }

    #[test]
    fn post_sum_master_limiter_handles_decks_samples_and_tts() {
        let meter = MasterMeter::new();
        let mut master = MasterMixer::new(48_000, meter.clone());
        let source = StereoFrame {
            left: 0.8,
            right: -0.8,
        };
        let mut peak = 0.0_f32;
        for _ in 0..8_000 {
            let output = master.process_frame(source, source, source, source);
            peak = peak.max(output.left.abs()).max(output.right.abs());
        }
        assert!(peak <= 1.0, "master output exceeded the PCM rail: {peak}");
        assert!(
            meter.gain_reduction_db() > 0.0,
            "master limiter did not report gain reduction"
        );
        assert!(meter.limiter_latency_ms() >= 5.0);
    }

    #[test]
    fn sample_and_tts_auxiliary_inputs_reach_the_master_sum() {
        let meter = MasterMeter::new();
        let mut master = MasterMixer::new(48_000, meter.clone());
        let mut output = StereoFrame::default();
        for _ in 0..8_000 {
            output = master.process_frame(
                StereoFrame::default(),
                StereoFrame::default(),
                StereoFrame {
                    left: 0.2,
                    right: -0.2,
                },
                StereoFrame {
                    left: 0.3,
                    right: -0.3,
                },
            );
        }
        assert!((output.left - 0.5).abs() < 0.02);
        assert!((output.right + 0.5).abs() < 0.02);
        assert_eq!(meter.gain_reduction_db(), 0.0);
    }

    #[test]
    fn unity_deck_does_not_create_unnecessary_reduction() {
        let meter = MasterMeter::new();
        let mut master = MasterMixer::new(48_000, meter.clone());
        for _ in 0..8_000 {
            let _ = master.process_frame(
                StereoFrame {
                    left: 0.2,
                    right: 0.1,
                },
                StereoFrame::default(),
                StereoFrame::default(),
                StereoFrame::default(),
            );
        }
        assert_eq!(meter.gain_reduction_db(), 0.0);
    }
}
