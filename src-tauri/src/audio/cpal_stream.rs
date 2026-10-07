//! Dynamic hardware-buffer recovery and audio-glitch telemetry for the single CPAL output stream.
//!
//! A driver xrun cannot be fixed inside the audio callback: the hardware buffer size is a property
//! of the *stream*, so recovering means rebuilding the stream with a larger buffer. This module
//! owns the parts that must stay testable and realtime-safe:
//!
//! * [`CallbackWatchdog`] turns callback timing into a glitch verdict. It exists because the two
//!   shipped platforms report glitches differently:
//!   * **Android (AAudio)** — the backend compares `AAudioStream_getXRunCount` on every data
//!     callback and forwards a `cpal::ErrorKind::Xrun` to the error callback, so real xruns are
//!     visible directly;
//!   * **Windows (WASAPI)** — the 0.18 backend only raises `Xrun` on the *capture* path
//!     (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`) and pre-fills the render buffer with silence, so
//!     an output underrun is silent in both senses. The watchdog detects it instead by comparing
//!     the wall-clock interval between callbacks with the interval the handed-out frame count
//!     implies: a late callback means the device ran out of data, an early one means it discarded
//!     data it had already been given.
//! * [`BufferRecoveryPolicy`] decides when a bigger buffer is worth a stream rebuild. Growth
//!   doubles the buffer, is capped by what the device advertises, is rate-limited by a cooldown,
//!   and is never shrunk again: once a device has proven unstable, trading latency for stability
//!   is the right call.
//! * [`GlitchTelemetry`] keeps lock-free counters for the data callback plus a bounded, readable
//!   event log that the status command exposes as internal telemetry.
//!
//! Everything the data or error callback touches is an atomic or a `try_lock`; the log is appended
//! from normal threads only.

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Smallest hardware buffer the recovery will ever ask for.
pub(super) const MIN_BUFFER_FRAMES: u32 = 128;
/// Largest hardware buffer the recovery will ask for, regardless of what the device advertises.
pub(super) const MAX_RECOVERED_BUFFER_FRAMES: u32 = 8_192;
/// Assumed buffer size before a callback has reported the real one.
pub(super) const NOMINAL_BUFFER_FRAMES: u32 = 1_024;
/// Recoverable glitches needed before the hardware buffer is doubled.
pub(super) const GLITCHES_BEFORE_GROWTH: u32 = 3;
/// Minimum delay between two rebuild attempts, in milliseconds.
pub(super) const GROWTH_COOLDOWN_MS: u64 = 5_000;
/// Number of event-log lines kept for the internal telemetry log.
pub(super) const TELEMETRY_LOG_CAPACITY: usize = 64;
/// Number of distinct [`GlitchKind`] values tracked per batch.
const GLITCH_KIND_COUNT: usize = 8;
/// A callback arriving later than `expected * NUMERATOR / DENOMINATOR` counts as an underrun.
const LATE_CALLBACK_NUMERATOR: u64 = 3;
const LATE_CALLBACK_DENOMINATOR: u64 = 2;
/// A callback arriving sooner than `expected / SHORT_CALLBACK_DENOMINATOR` counts as an overflow.
const SHORT_CALLBACK_DENOMINATOR: u64 = 2;
/// Intervals below a millisecond are too short to time reliably; the watchdog stays quiet.
const WATCHDOG_MIN_EXPECTED_NS: u64 = 1_000_000;
/// The first two seconds of a stream may be irregular while the device ramps up.
const WATCHDOG_STARTUP_GRACE_NS: u64 = 2_000_000_000;

/// What was observed on the output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum GlitchKind {
    /// An active deck had no rendered frame to hand to the device.
    Underrun,
    /// The driver took data before we produced it, or the capture tap dropped a frame.
    Overflow,
    /// The CPAL backend reported `ErrorKind::Xrun` for this stream.
    Xrun,
    /// The callback arrived late: the audio thread was starved or the device stalled.
    LateCallback,
    /// The backend reported a device error for this stream.
    DeviceError,
    /// A callback could not take the output graph because a stream rebuild held it.
    LockContention,
    /// The hardware buffer was doubled after repeated glitches.
    BufferGrown,
    /// The device is already at its maximum buffer size and cannot be grown further.
    BufferExhausted,
}

impl GlitchKind {
    /// Kinds that justify asking the device for a larger buffer.
    pub(super) fn is_buffer_recoverable(self) -> bool {
        matches!(
            self,
            Self::Underrun | Self::Overflow | Self::Xrun | Self::LateCallback
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Underrun => "underrun",
            Self::Overflow => "overflow",
            Self::Xrun => "xrun",
            Self::LateCallback => "late-callback",
            Self::DeviceError => "device-error",
            Self::LockContention => "lock-contention",
            Self::BufferGrown => "buffer-grown",
            Self::BufferExhausted => "buffer-exhausted",
        }
    }

    /// Position of the kind in the telemetry batch arrays.
    fn index(self) -> usize {
        match self {
            Self::Underrun => 0,
            Self::Overflow => 1,
            Self::Xrun => 2,
            Self::LateCallback => 3,
            Self::DeviceError => 4,
            Self::LockContention => 5,
            Self::BufferGrown => 6,
            Self::BufferExhausted => 7,
        }
    }

    fn bit(self) -> u64 {
        1 << self.index()
    }

    fn from_bit(bit: usize) -> Option<Self> {
        match bit {
            0 => Some(Self::Underrun),
            1 => Some(Self::Overflow),
            2 => Some(Self::Xrun),
            3 => Some(Self::LateCallback),
            4 => Some(Self::DeviceError),
            5 => Some(Self::LockContention),
            6 => Some(Self::BufferGrown),
            7 => Some(Self::BufferExhausted),
            _ => None,
        }
    }
}

/// Backend that owns the output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum GlitchSource {
    Wasapi,
    Aaudio,
    Alsa,
    CoreAudio,
    Asio,
    Jack,
    Other,
}

impl GlitchSource {
    /// The backend the shipped build actually uses on this target.
    pub(super) fn detect() -> Self {
        #[cfg(target_os = "windows")]
        {
            Self::Wasapi
        }
        #[cfg(target_os = "android")]
        {
            Self::Aaudio
        }
        #[cfg(target_os = "macos")]
        {
            Self::CoreAudio
        }
        #[cfg(not(any(target_os = "windows", target_os = "android", target_os = "macos")))]
        {
            match cpal::default_host().id().name() {
                "alsa" => Self::Alsa,
                "jack" => Self::Jack,
                "asio" => Self::Asio,
                _ => Self::Other,
            }
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Wasapi => "WASAPI",
            Self::Aaudio => "AAudio",
            Self::Alsa => "ALSA",
            Self::CoreAudio => "CoreAudio",
            Self::Asio => "ASIO",
            Self::Jack => "JACK",
            Self::Other => "CPAL",
        }
    }
}

/// Expected callback interval for a frame count, in nanoseconds.
pub(super) fn expected_interval_ns(frames: u32, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (u64::from(frames) * 1_000_000_000) / u64::from(sample_rate)
}

/// Turns the timing of consecutive data callbacks into a glitch verdict.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallbackWatchdog {
    first_callback_ns: Option<u64>,
    last_callback_ns: Option<u64>,
}

impl CallbackWatchdog {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Forgets all timing state. Called after the stream is rebuilt, because a rebuild introduces a
    /// deliberately long gap between callbacks that is not a glitch.
    pub(super) fn reset(&mut self) {
        self.first_callback_ns = None;
        self.last_callback_ns = None;
    }

    /// Records one data callback and reports what its timing implies.
    pub(super) fn observe(
        &mut self,
        now_ns: u64,
        frames: u32,
        sample_rate: u32,
    ) -> Option<GlitchKind> {
        let previous_ns = self.last_callback_ns.replace(now_ns);
        let first_ns = *self.first_callback_ns.get_or_insert(now_ns);
        let expected_ns = expected_interval_ns(frames, sample_rate);
        let previous_ns = previous_ns?;
        if now_ns.saturating_sub(first_ns) < WATCHDOG_STARTUP_GRACE_NS
            || expected_ns < WATCHDOG_MIN_EXPECTED_NS
        {
            return None;
        }
        let interval_ns = now_ns.saturating_sub(previous_ns);
        if interval_ns.saturating_mul(SHORT_CALLBACK_DENOMINATOR) < expected_ns {
            return Some(GlitchKind::Overflow);
        }
        if interval_ns.saturating_mul(LATE_CALLBACK_DENOMINATOR)
            > expected_ns.saturating_mul(LATE_CALLBACK_NUMERATOR)
        {
            return Some(GlitchKind::LateCallback);
        }
        None
    }
}

/// Decides whether the hardware buffer should be enlarged after glitches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BufferRecoveryPolicy {
    base_frames: u32,
    max_frames: u32,
    frames: u32,
    pending_frames: Option<u32>,
    glitches_since_growth: u32,
    growths: u32,
    last_attempt_ms: Option<u64>,
    exhausted: bool,
}

impl BufferRecoveryPolicy {
    /// `device_default_frames` is the buffer size the device reported (when it reports one) and
    /// `device_max_frames` the largest size it accepts; both are optional because
    /// `cpal::SupportedBufferSize::Unknown` is common, especially on Android.
    pub(super) fn new(device_default_frames: Option<u32>, device_max_frames: Option<u32>) -> Self {
        let base = device_default_frames
            .unwrap_or(NOMINAL_BUFFER_FRAMES)
            .max(MIN_BUFFER_FRAMES);
        let max = device_max_frames
            .unwrap_or(MAX_RECOVERED_BUFFER_FRAMES)
            .clamp(base, MAX_RECOVERED_BUFFER_FRAMES);
        Self {
            base_frames: base,
            max_frames: max,
            frames: base,
            pending_frames: None,
            glitches_since_growth: 0,
            growths: 0,
            last_attempt_ms: None,
            exhausted: false,
        }
    }

    pub(super) fn frames(&self) -> u32 {
        self.frames
    }

    pub(super) fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Syncs the policy with the frame count the device actually hands out. That value is the only
    /// reliable base buffer size on hosts that report `SupportedBufferSize::Unknown`, which is the
    /// common case on Android.
    pub(super) fn note_observed_frames(&mut self, frames: u32) {
        if frames == 0 || self.pending_frames.is_some() {
            return;
        }
        let observed = frames.clamp(MIN_BUFFER_FRAMES, self.max_frames);
        if observed > self.frames {
            self.frames = observed;
        }
    }

    /// Feeds newly observed recoverable glitches and returns the buffer size to rebuild with, when
    /// this batch justifies a rebuild.
    pub(super) fn plan_growth(&mut self, glitches: u32, now_ms: u64) -> Option<u32> {
        if self.exhausted || self.pending_frames.is_some() {
            return None;
        }
        self.glitches_since_growth = self.glitches_since_growth.saturating_add(glitches);
        if self.glitches_since_growth < GLITCHES_BEFORE_GROWTH {
            return None;
        }
        if self
            .last_attempt_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < GROWTH_COOLDOWN_MS)
        {
            return None;
        }
        let next = self
            .frames
            .saturating_mul(2)
            .min(self.max_frames)
            .max(self.base_frames);
        self.last_attempt_ms = Some(now_ms);
        self.glitches_since_growth = 0;
        if next <= self.frames {
            self.exhausted = true;
            return None;
        }
        self.pending_frames = Some(next);
        Some(next)
    }

    /// Records the outcome of a rebuild attempt. A failure clears the plan so a later batch of
    /// glitches may retry once the cooldown has passed.
    pub(super) fn note_growth(&mut self, succeeded: bool) {
        if let Some(frames) = self.pending_frames.take() {
            if succeeded {
                self.frames = frames;
                self.growths = self.growths.saturating_add(1);
            }
        }
    }
}

/// One line of the internal glitch telemetry log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GlitchEvent {
    /// Milliseconds since the master bus opened, on a monotonic clock.
    pub at_ms: u64,
    pub kind: GlitchKind,
    pub source: GlitchSource,
    pub detail: String,
}

impl GlitchEvent {
    /// Human-readable line, as exposed to the status command.
    pub(super) fn line(&self) -> String {
        format!(
            "[audio-glitch] t+{}ms backend={} kind={} {}",
            self.at_ms,
            self.source.as_str(),
            self.kind.as_str(),
            self.detail
        )
    }
}

/// Glitch counters plus the bounded telemetry log.
pub(super) struct GlitchTelemetry {
    source: GlitchSource,
    started: Instant,
    underruns: AtomicU64,
    overflows: AtomicU64,
    xruns: AtomicU64,
    late_callbacks: AtomicU64,
    device_errors: AtomicU64,
    lock_contentions: AtomicU64,
    buffer_growths: AtomicU64,
    buffer_exhausted: AtomicBool,
    callback_frames: AtomicU32,
    current_buffer_frames: AtomicU32,
    /// Bitmask and per-kind counts of the glitches observed since the last drain, gathered by the
    /// data callback and consumed by a normal thread.
    pending_kinds: AtomicU64,
    pending_counts: [AtomicU64; GLITCH_KIND_COUNT],
    log: Mutex<VecDeque<GlitchEvent>>,
}

impl GlitchTelemetry {
    pub(super) fn new(source: GlitchSource, buffer_frames: u32) -> Arc<Self> {
        Arc::new(Self {
            source,
            started: Instant::now(),
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            late_callbacks: AtomicU64::new(0),
            device_errors: AtomicU64::new(0),
            lock_contentions: AtomicU64::new(0),
            buffer_growths: AtomicU64::new(0),
            buffer_exhausted: AtomicBool::new(false),
            callback_frames: AtomicU32::new(buffer_frames),
            current_buffer_frames: AtomicU32::new(buffer_frames),
            pending_kinds: AtomicU64::new(0),
            pending_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            log: Mutex::new(VecDeque::with_capacity(TELEMETRY_LOG_CAPACITY)),
        })
    }

    fn counter(&self, kind: GlitchKind) -> Option<&AtomicU64> {
        match kind {
            GlitchKind::Underrun => Some(&self.underruns),
            GlitchKind::Overflow => Some(&self.overflows),
            GlitchKind::Xrun => Some(&self.xruns),
            GlitchKind::LateCallback => Some(&self.late_callbacks),
            GlitchKind::DeviceError => Some(&self.device_errors),
            GlitchKind::LockContention => Some(&self.lock_contentions),
            GlitchKind::BufferGrown => Some(&self.buffer_growths),
            // Exhaustion is not a recurring event: it is a state flag and a single log line.
            GlitchKind::BufferExhausted => None,
        }
    }

    /// Monotonic milliseconds since the master bus opened; the recovery policy's clock.
    pub(super) fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    pub(super) fn current_buffer_frames(&self) -> u32 {
        self.current_buffer_frames.load(Ordering::Acquire)
    }

    pub(super) fn observed_callback_frames(&self) -> u32 {
        self.callback_frames.load(Ordering::Acquire)
    }

    pub(super) fn buffer_exhausted(&self) -> bool {
        self.buffer_exhausted.load(Ordering::Acquire)
    }

    /// Callback-side: bumps a counter and remembers the kind for the next log drain. Lock-free and
    /// allocation-free, so it is safe on the audio thread.
    pub(super) fn record(&self, kind: GlitchKind) {
        self.record_many(kind, 1);
    }

    /// Callback-side: same as [`Self::record`] for a batch observed in one callback.
    pub(super) fn record_many(&self, kind: GlitchKind, count: u64) {
        if count == 0 {
            return;
        }
        if let Some(counter) = self.counter(kind) {
            counter.fetch_add(count, Ordering::Relaxed);
        }
        let index = kind.index();
        if let Some(pending) = self.pending_counts.get(index) {
            pending.fetch_add(count, Ordering::Relaxed);
        }
        self.pending_kinds.fetch_or(kind.bit(), Ordering::Relaxed);
    }

    /// Callback-side: publishes the frame count the device handed to this callback.
    pub(super) fn observe_callback_frames(&self, frames: u32) {
        self.callback_frames.fetch_max(frames, Ordering::Relaxed);
    }

    /// Normal-thread: records the buffer size the stream is running with.
    pub(super) fn note_buffer_frames(&self, frames: u32) {
        self.current_buffer_frames.store(frames, Ordering::Release);
        self.callback_frames.fetch_max(frames, Ordering::Relaxed);
    }

    /// Normal-thread: the device cannot be given a larger buffer; log it once.
    pub(super) fn note_buffer_exhausted(&self) {
        if self.buffer_exhausted.swap(true, Ordering::AcqRel) {
            return;
        }
        self.log_event(
            GlitchKind::BufferExhausted,
            format!(
                "device maximum of {} frames reached; further glitches cannot grow the buffer",
                self.current_buffer_frames()
            ),
        );
    }

    /// Normal-thread: appends one log line. Never called from the audio callback.
    pub(super) fn log_event(&self, kind: GlitchKind, detail: String) {
        let event = GlitchEvent {
            at_ms: self.now_ms(),
            kind,
            source: self.source,
            detail,
        };
        let Ok(mut log) = self.log.lock() else {
            return;
        };
        while log.len() >= TELEMETRY_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(event);
    }

    /// Normal-thread: turns the glitches recorded since the last call into log lines and returns
    /// how many of them justify growing the hardware buffer.
    pub(super) fn drain_recoverable_glitches(&self) -> u32 {
        let kinds = self.pending_kinds.swap(0, Ordering::AcqRel);
        if kinds == 0 {
            return 0;
        }
        let mut recoverable: u32 = 0;
        for index in 0..GLITCH_KIND_COUNT {
            if kinds & (1 << index) == 0 {
                continue;
            }
            let Some(kind) = GlitchKind::from_bit(index) else {
                continue;
            };
            let observed = self
                .pending_counts
                .get(index)
                .map(|pending| pending.swap(0, Ordering::AcqRel))
                .unwrap_or(0);
            let total = self
                .counter(kind)
                .map(|counter| counter.load(Ordering::Relaxed))
                .unwrap_or(0);
            if kind.is_buffer_recoverable() {
                let clamped = observed.min(u64::from(u32::MAX)) as u32;
                recoverable = recoverable.saturating_add(clamped);
            }
            self.log_event(
                kind,
                format!(
                    "observed={observed} total={total} bufferFrames={} callbackFrames={}",
                    self.current_buffer_frames(),
                    self.observed_callback_frames()
                ),
            );
        }
        recoverable
    }

    /// Normal-thread: a snapshot for the status command.
    pub(super) fn snapshot(&self) -> GlitchSnapshot {
        GlitchSnapshot {
            backend: self.source.as_str().to_owned(),
            underruns: self.underruns.load(Ordering::Relaxed),
            overflows: self.overflows.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            late_callbacks: self.late_callbacks.load(Ordering::Relaxed),
            device_errors: self.device_errors.load(Ordering::Relaxed),
            lock_contentions: self.lock_contentions.load(Ordering::Relaxed),
            buffer_growths: self.buffer_growths.load(Ordering::Relaxed),
            buffer_exhausted: self.buffer_exhausted(),
            current_buffer_frames: self.current_buffer_frames(),
            callback_frames: self.observed_callback_frames(),
            log: self
                .log
                .lock()
                .map(|log| log.iter().map(GlitchEvent::line).collect())
                .unwrap_or_default(),
        }
    }
}

/// Serializable view of the glitch telemetry, exposed through `get_audio_status`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GlitchSnapshot {
    pub backend: String,
    pub underruns: u64,
    pub overflows: u64,
    pub xruns: u64,
    pub late_callbacks: u64,
    pub device_errors: u64,
    pub lock_contentions: u64,
    pub buffer_growths: u64,
    pub buffer_exhausted: bool,
    pub current_buffer_frames: u32,
    pub callback_frames: u32,
    /// Most recent glitch log lines, oldest first.
    pub log: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        expected_interval_ns, BufferRecoveryPolicy, CallbackWatchdog, GlitchKind, GlitchSource,
        GlitchTelemetry, GLITCHES_BEFORE_GROWTH, GROWTH_COOLDOWN_MS, MAX_RECOVERED_BUFFER_FRAMES,
        NOMINAL_BUFFER_FRAMES, TELEMETRY_LOG_CAPACITY,
    };

    /// A monotonic base far enough from zero that the watchdog's startup grace has to be earned.
    const BASE_NS: u64 = 1_000_000_000_000;

    #[test]
    fn expected_interval_matches_the_frame_count() {
        assert_eq!(expected_interval_ns(480, 48_000), 10_000_000);
        assert_eq!(expected_interval_ns(1_024, 44_100), 23_219_954);
        assert_eq!(expected_interval_ns(512, 0), 0);
    }

    #[test]
    fn watchdog_tolerates_startup_then_reports_late_and_short_callbacks() {
        let mut watchdog = CallbackWatchdog::new();
        // First callback: there is no interval to measure yet.
        assert_eq!(watchdog.observe(BASE_NS, 512, 48_000), None);
        // Irregular ramp-up inside the grace window never produces a verdict.
        assert_eq!(watchdog.observe(BASE_NS + 40_000_000, 512, 48_000), None);
        // Past the grace window the 2.45 s gap is a genuine late callback.
        let mut now = BASE_NS + 2_500_000_000;
        assert_eq!(
            watchdog.observe(now, 512, 48_000),
            Some(GlitchKind::LateCallback)
        );
        for _ in 0..5 {
            now += 10_666_666;
            assert_eq!(watchdog.observe(now, 512, 48_000), None);
        }
        // Three times the expected interval: the device ran out of data.
        now += 32_000_000;
        assert_eq!(
            watchdog.observe(now, 512, 48_000),
            Some(GlitchKind::LateCallback)
        );
        // Half an interval early: the driver took data we had not produced yet.
        now += 4_000_000;
        assert_eq!(
            watchdog.observe(now, 512, 48_000),
            Some(GlitchKind::Overflow)
        );
    }

    #[test]
    fn watchdog_scales_with_the_frame_count_the_device_asks_for() {
        let mut watchdog = CallbackWatchdog::new();
        assert_eq!(watchdog.observe(BASE_NS, 1_024, 48_000), None);
        let mut now = BASE_NS + 2_500_000_000;
        assert_eq!(
            watchdog.observe(now, 1_024, 48_000),
            Some(GlitchKind::LateCallback)
        );
        for _ in 0..3 {
            now += 21_333_333;
            assert_eq!(watchdog.observe(now, 1_024, 48_000), None);
        }
        // The device now asks for a quarter of the frames: a 5.3 ms cadence is correct for it...
        now += 5_333_333;
        assert_eq!(watchdog.observe(now, 256, 48_000), None);
        // ...and a 1 ms cadence means it already consumed frames that were not there.
        now += 1_000_000;
        assert_eq!(
            watchdog.observe(now, 256, 48_000),
            Some(GlitchKind::Overflow)
        );
    }

    #[test]
    fn watchdog_reset_forgets_the_previous_stream() {
        let mut watchdog = CallbackWatchdog::new();
        assert_eq!(watchdog.observe(BASE_NS, 512, 48_000), None);
        assert_eq!(
            watchdog.observe(BASE_NS + 2_500_000_000, 512, 48_000),
            Some(GlitchKind::LateCallback)
        );
        watchdog.reset();
        assert_eq!(watchdog.observe(BASE_NS + 2_600_000_000, 512, 48_000), None);
    }

    #[test]
    fn policy_waits_for_enough_glitches_and_then_doubles() {
        let mut policy = BufferRecoveryPolicy::new(Some(512), Some(4_096));
        assert_eq!(policy.frames(), 512);
        assert_eq!(policy.plan_growth(1, 0), None);
        assert_eq!(policy.plan_growth(GLITCHES_BEFORE_GROWTH - 2, 10), None);
        assert_eq!(policy.plan_growth(1, 20), Some(1_024));
        // A rebuild is already planned: no second plan until it reports back.
        assert_eq!(policy.plan_growth(50, 30), None);
        assert_eq!(policy.pending_frames, Some(1_024));
        policy.note_growth(true);
        assert_eq!(policy.frames(), 1_024);
        assert_eq!(policy.growths, 1);
        // Each further attempt has to wait out the cooldown after the previous one.
        assert_eq!(
            policy.plan_growth(GLITCHES_BEFORE_GROWTH, GROWTH_COOLDOWN_MS + 1_000),
            Some(2_048)
        );
        policy.note_growth(true);
        assert_eq!(
            policy.plan_growth(GLITCHES_BEFORE_GROWTH, GROWTH_COOLDOWN_MS * 2 + 2_000),
            Some(4_096)
        );
        policy.note_growth(true);
        // The device maximum is reached: further glitches cannot grow the buffer.
        assert_eq!(policy.plan_growth(1_000, GROWTH_COOLDOWN_MS * 4), None);
        assert!(policy.exhausted());
        assert_eq!(policy.frames(), 4_096);
    }

    #[test]
    fn policy_adopts_the_frame_count_the_device_actually_asks_for() {
        let mut policy = BufferRecoveryPolicy::new(Some(512), Some(4_096));
        // A smaller callback than the advertised default does not lower the target.
        policy.note_observed_frames(256);
        assert_eq!(policy.frames(), 512);
        policy.note_observed_frames(1_024);
        assert_eq!(policy.frames(), 1_024);
        // A device that hands out more than it advertised is clamped to the hard ceiling.
        policy.note_observed_frames(MAX_RECOVERED_BUFFER_FRAMES * 4);
        assert_eq!(policy.frames(), MAX_RECOVERED_BUFFER_FRAMES);
        assert_eq!(policy.plan_growth(GLITCHES_BEFORE_GROWTH, 0), None);
        assert!(policy.exhausted());
    }

    #[test]
    fn policy_rate_limits_rebuild_attempts_and_retries_after_a_failure() {
        let mut policy = BufferRecoveryPolicy::new(None, None);
        assert_eq!(policy.base_frames, NOMINAL_BUFFER_FRAMES);
        assert_eq!(policy.max_frames, MAX_RECOVERED_BUFFER_FRAMES);
        assert_eq!(policy.plan_growth(GLITCHES_BEFORE_GROWTH, 0), Some(2_048));
        policy.note_growth(false);
        assert_eq!(policy.frames(), NOMINAL_BUFFER_FRAMES);
        assert_eq!(policy.growths, 0);
        // Too soon after the failed attempt.
        assert_eq!(policy.plan_growth(10, GROWTH_COOLDOWN_MS - 1), None);
        assert_eq!(policy.plan_growth(0, GROWTH_COOLDOWN_MS + 1), Some(2_048));
        policy.note_growth(true);
        assert_eq!(policy.growths, 1);
        assert_eq!(policy.pending_frames, None);
    }

    #[test]
    fn policy_clamps_a_device_maximum_that_is_smaller_than_the_base() {
        let mut policy = BufferRecoveryPolicy::new(Some(256), Some(64));
        assert_eq!(policy.base_frames, 256);
        assert_eq!(policy.max_frames, 256);
        assert_eq!(policy.plan_growth(GLITCHES_BEFORE_GROWTH, 0), None);
        assert!(policy.exhausted());
    }

    #[test]
    fn telemetry_counts_drains_and_logs_glitches() {
        let telemetry = GlitchTelemetry::new(GlitchSource::Wasapi, 512);
        telemetry.record_many(GlitchKind::Underrun, 2);
        telemetry.record(GlitchKind::DeviceError);
        telemetry.observe_callback_frames(1_024);
        let recoverable = telemetry.drain_recoverable_glitches();
        assert_eq!(
            recoverable, 2,
            "device errors do not justify a bigger buffer"
        );
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.underruns, 2);
        assert_eq!(snapshot.device_errors, 1);
        assert_eq!(snapshot.callback_frames, 1_024);
        assert_eq!(snapshot.backend, "WASAPI");
        assert_eq!(snapshot.log.len(), 2);
        assert!(snapshot.log[0].starts_with("[audio-glitch] t+"));
        assert!(snapshot.log[0].contains("kind=underrun"));
        assert!(snapshot.log[0].contains("observed=2"));
        // Counters are cumulative, but each batch is only logged once.
        assert_eq!(telemetry.drain_recoverable_glitches(), 0);
        assert_eq!(telemetry.snapshot().log.len(), 2);
    }

    #[test]
    fn telemetry_log_is_bounded_and_reports_growth_and_exhaustion() {
        let telemetry = GlitchTelemetry::new(GlitchSource::Aaudio, 256);
        for _ in 0..TELEMETRY_LOG_CAPACITY + 10 {
            telemetry.log_event(GlitchKind::Xrun, "backend xrun".to_owned());
        }
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.log.len(), TELEMETRY_LOG_CAPACITY);
        assert_eq!(snapshot.xruns, 0, "logging does not change the counters");
        telemetry.note_buffer_frames(2_048);
        telemetry.note_buffer_exhausted();
        telemetry.note_buffer_exhausted();
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.current_buffer_frames, 2_048);
        assert!(snapshot.buffer_exhausted);
        assert_eq!(snapshot.callback_frames, 2_048);
        assert_eq!(
            telemetry.drain_recoverable_glitches(),
            0,
            "exhaustion is not recoverable"
        );
        assert!(snapshot
            .log
            .iter()
            .any(|line| line.contains("kind=buffer-exhausted")));
        let after = telemetry.snapshot();
        assert_eq!(
            after
                .log
                .iter()
                .filter(|line| line.contains("kind=buffer-exhausted"))
                .count(),
            1,
            "exhaustion is logged exactly once"
        );
    }
}
