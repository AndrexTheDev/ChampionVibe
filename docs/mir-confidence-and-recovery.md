# MIR confidence, grid editing and CPAL buffer recovery

This document describes the three additions behind the 0.7-confidence policy, the manual grid
editor and the native output-stream recovery path. All values stay local; no audio or metadata
leaves the device.

## 1. Confidence policy

The shared policy lives in `src-tauri/src/confidence.rs`:

| Constant | Value | Meaning |
| --- | --- | --- |
| `TRUSTED_CONFIDENCE` | `0.7` | at or above this the result may be presented as trusted |
| `UNSURE_CONFIDENCE_CEILING` | `0.69` | hard ceiling for a heuristic that could not corroborate itself |

The importer records two machine-listening confidences per track:

* `bpm_confidence` — how far the beat-grid estimator trusts its BPM and grid.
* `key_confidence` — how far the key estimator trusts `musical_key`.

`heuristic_confidence(raw, corroborated)` is the only way a heuristic may publish a value. A
heuristic that cannot corroborate its own answer (the same beat period in both halves of the track,
or at least three pitch classes carrying the chroma) is pinned below the trusted threshold, so
"unsure" is enforced in the DSP layer rather than being left to the UI. Embedded BPM/key tags and
manual corrections are reported with `trusted_confidence()` (`1.0`) because they come from an
authoritative source.

`src-tauri/src/import/analysis.rs` implements the corroboration checks: `dominant_lag` and
`same_tempo` compare the tempo evidence of both halves, `tempo_is_corroborated` requires at least 16
beats with both halves agreeing, and the key heuristic requires `MIN_KEY_PITCH_CLASSES` significant
chroma bins. A tempo whose best lag scores below `0.03` is reported as "no grid" rather than as a
guess.

## 2. SQLite migration

`tracks` gains three additive columns through `ensure_track_metadata_columns`, which is idempotent
and safe on existing databases:

```sql
bpm_confidence REAL NOT NULL DEFAULT 0.0
key_confidence REAL NOT NULL DEFAULT 0.0
is_user_verified INTEGER NOT NULL DEFAULT 0
```

* `audio_analysis` and `beat_grids` keep the analysed values; `tracks` mirrors the confidences and
  the verification flag so the paged library query can render a badge without joining.
* `commit_import_result` writes the confidences behind an `already_verified` guard: re-importing a
  track that a human already verified never overwrites the verified BPM, key or grid.
* `search_tracks_page_with_filters` and `load_mix_track` expose the three fields (positions 22–24
  and 19–21).

## 3. Manual corrections

`apply_grid_correction` (SQLite) and `correct_track_grid` (Tauri command) implement one save:

1. Validation: BPM inside `CORRECTION_BPM_RANGE` (30–300), grid shift within
   `MAX_GRID_SHIFT_MS` (±2000 ms), key parseable and camelot code a real wheel position.
2. The corrected grid is re-spaced from the stored beats around the fixed downbeat
   (`respace_beats`), then shifted; beats that would land before the track start are dropped.
3. `beat_grids`, `audio_analysis` and `tracks` are updated in one immediate transaction, the
   confidences of the touched fields move to `1.0` and `is_user_verified` becomes `1`.
4. The cached `waveform_peak_cache` row is deleted, because the waveform packet embeds the beat
   markers.
5. Live adoption: the deck that holds the track swaps its grid in place
   (`EngineCommand::SetGrid`, no decoder or stream rebuild, playback keeps running), and the mix
   engine re-reads its in-memory record and re-prepares the next transition. A deck releases its
   library binding when it stops, so a correction can never land on a track the deck no longer
   holds. Failures to adopt are reported as `adoptionWarning` while the correction itself stays
   durable.

The outcome carries the corrected `bpm`, `musicalKey`, `camelotKey`, `beats`, both confidences and
`isUserVerified`, plus `liveDeckUpdated`, `mixEngineUpdated`, `compatibleCamelot` and
`adoptionWarning`.

## 4. Grid editor UI

`src/ui/grid_editor.js` holds every rule in pure, Node-testable helpers and one `initGridEditor`
wiring function:

* `camelotForMusicalKey`, `musicalKeyForCamelot`, `compatibleCamelotCodes`, `transposeCamelot` —
  the Camelot wheel, mirroring `src-tauri/src/camelot.rs` (including the four cases both test
  suites assert).
* `nudgeBpm` / `clampBpm` — ±0.1 BPM nudging, one-decimal precision, 30–300 BPM.
* `clampGridShiftMs` / `shiftBeats` — whole-millisecond shifts with ±1 ms and ±10 ms steps.
* `confidenceBand`, `confidenceBadge`, `confidenceTooltip` — the badge tones: `verified` (green),
  `trusted` (green), `heuristic` (amber/orange) and `unknown` (grey).
* `buildCorrection` — sends only the values the user actually touched.
* `applyCorrectionToTrack` / `replaceTrackInPages` — update the cached library pages in place.

The modal markup lives in `index.html` (`#grid-editor-modal`), the styling in `src/styles.css`, and
all copy in the four locale files. `src/ui/grid_editor.test.js` covers the wheel, the nudges, the
bands, the payload and the cached-row merge; `src/i18n.test.js` additionally scans the module for
translated copy.

## 5. CPAL buffer recovery and glitch telemetry

`src-tauri/src/audio/cpal_stream.rs` owns the recovery and telemetry logic; `master_bus.rs` is the
only module that talks to CPAL and applies it.

* `GlitchKind` distinguishes underruns, overflows, xruns, late callbacks, device errors, lock
  contention, buffer growth and buffer exhaustion.
* `GlitchSource::detect()` records the active backend (WASAPI, AAudio, CoreAudio, else ALSA). WASAPI
  only reports xruns on capture and AAudio exposes `x_run_count`, so `CallbackWatchdog` derives
  underruns and overruns from the callback interval itself: a callback arriving less than half the
  expected interval after the previous one is an overflow, more than one and a half times later a
  late callback. A two-second grace period covers stream start-up.
* `BufferRecoveryPolicy` grows the requested `BufferSize::Fixed` frame count after
  `GLITCHES_BEFORE_GROWTH` recoverable glitches, respects a `GROWTH_COOLDOWN_MS` cooldown and caps
  the growth at `MAX_RECOVERED_BUFFER_FRAMES`. `note_observed_frames` adopts the frame count the
  device actually uses when it differs from the requested one, and never lowers the target.
* `GlitchTelemetry` is a lock-free ring (per-kind atomics plus a bounded log of
  `[audio-glitch] t+{ms}ms backend=… kind=…` lines) that can be drained from the control thread and
  snapshotted into `AudioStatus.audioGlitches` for the UI.

All counters are plain atomics: the recovery path never blocks or allocates inside the audio
callback.
