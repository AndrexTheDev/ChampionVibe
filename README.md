# ChampionVibe

Cross-platform Tauri v2 foundation for Windows 10 and Android, with a Vanilla JS/CSS interface, Rust commands, a local SQLite library, and a separately deployable license issuer.

> **Security and cost reality:** no client application can be made “uncrackable.” A device owner can patch its binary, alter its clock, or emulate identifiers. This repository does not claim otherwise. It keeps the Ed25519 **private signing key off the client**, verifies signed entitlements in Rust, and makes the issuer perform authoritative chain checks. Public RPC/oracle services currently require no API key, but are rate-limited and have no availability guarantee. Self-hosting the issuer on your own hardware can avoid hosting fees; it cannot guarantee that all operation is always free.

## Project layout

```text
ChampionVibe/
├── Cargo.toml                         # Rust workspace
├── package.json                       # Vanilla frontend + Tauri CLI
├── vite.config.js
├── index.html
├── src/
│   ├── main.js                        # Tauri IPC, quote/activation UI, wallet detection
│   ├── i18n.js + locales/{de,en,es,fr}.json # zero-dependency, no-reload UI translations
│   ├── audio-format.js + waveform.js   # local codec badges and binary peak-packet parser
│   └── styles.css
├── crates/
│   ├── licensing-core/                # Shared Chainlink quotes, Ed25519 and chain-verifier code
│   │   └── src/{lib,model,license,oracle,rpc}.rs
│   └── device-identity/               # Kotlin Android ID / AAudio context bridge
│       ├── android/                   # Android library, settings.gradle and Kotlin plugin class
│       └── src/
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   └── src/
│       ├── audio/                     # CPAL output, SPSC stem streaming, DSP, model registry and RAM samples
│       ├── import/                    # Local batch analysis, HT-Demucs/MDX-Net ONNX and Whisper workers
│       └── {lib,main,commands,database,device_binding,riddim,tts,localization,media_source,symphonia_support}.rs
├── docs/model-registry.md             # pinned model revisions, SHA-256 pins, tensor contracts and rights gates
└── license-issuer/
    └── src/main.rs                    # Self-hosted quote and activation service
```

## Build the Tauri app

Prerequisites: Rust stable (1.85+ for CPAL/Symphonia), Node.js/npm, the platform's C/C++ build tools and WebView2 runtime on Windows, and Android Studio + Android SDK/NDK for Android. Tauri's [platform prerequisites](https://tauri.app/start/prerequisites/) apply.

```sh
npm install
npm run tauri dev
npm run tauri build
```

Android is initialized once, then built/run using the Tauri v2 CLI:

```sh
npm run tauri android init
npm run tauri android dev
npm run tauri android build -- --apk
```

The Android build uses the same Rust crate and Tauri v2 mobile entry point. The minimum Android API is 26 (Android 8.0), matching CPAL's native AAudio backend. A local `tauri-plugin-device-identity` Tauri plugin bridges `Settings.Secure.ANDROID_ID` from Kotlin to Rust using `run_mobile_plugin`. Before opening CPAL on Android, that plugin passes a retained `Application` context to the Rust AAudio bridge; this is required because the Tauri app is embedded in a JVM process. Android imports additionally use `tauri-plugin-android-fs` for SAF URI and folder handling. The device-identity plugin's `android/settings.gradle` includes Tauri's `tauri-android` project. No audio samples or DSP are processed by the frontend; the Vite UI can still be previewed in a browser, but native commands intentionally remain disabled outside Tauri.

## Local database

The app creates `championvibe.sqlite` in Tauri's app-data directory. SQLite is built in via `rusqlite` and configured with WAL, `synchronous=NORMAL`, foreign keys, an in-memory temp store, a 64 MiB page cache and a 256 MiB mmap target. The indexed schema stores per-track codec, estimated average bitrate, source sample rate/channel count and source tags, plus beat-grid JSON/BPM, sentiment scores, stem paths, recording metadata (format, sample rate, channels, bit depth, bitrate, file size and completion state) and the signed license. An idempotent `track_analysis` migration adds integrated LUFS, true-peak dBTP, −14 LUFS gain offset, heuristic mood/rhythm/style labels and a 64-byte signed-int8 feature BLOB; mood, rhythm, style and loudness have dedicated indexes, and the primary key supports point reads. Metadata upgrades add these columns to older databases. It is designed for libraries well beyond 50,000 metadata rows; it does not load a whole library into the frontend.

`upsert_track`, `search_tracks`, and `search_tracks_page` are Rust/Tauri commands. The paged command accepts structured mood, rhythm, key, BPM-range and LUFS-range filters alongside title/artist/tag text; the Vanilla frontend parses natural queries such as `sad dembow 95 bpm` before sending them to SQLite. It returns a total plus a bounded offset/limit result; the frontend requests 200 tracks at a time and recycles at most 15 row nodes, keeping DOM and page data bounded while browsing libraries of 50,000+ tracks. `save_recording_metadata` is license-gated in the backend. Track metadata is not treated as a recording entitlement. `get_localized_catalog` returns message codes and German, English, Spanish and French text for import status/stages, recording status and metadata labels; import-job IPC snapshots include both stable codes and the localized message bundle.

## Local import, analysis and separation pipeline

The Rust worker in `src-tauri/src/import/` accepts local audio files or folders and caps a batch at 50,000 tracks. It stores job/item state and the local model paths in SQLite, updates job counters with SQLite triggers, supports cancellation, and processes tracks sequentially so model memory is reused. A track is committed only after its local analysis and stem files complete. Windows and Android folder enumeration use audio-container signatures instead of trusting filename extensions; Symphonia probes the content with no extension hint. The enabled first-party Symphonia features cover AAC/ADTS, AIFF/AIFC, ALAC, CAF, FLAC, MP3, Ogg/Vorbis, WAV, ISO/MP4 and MKV/WebM; Opus uses the third-party `symphonia-adapter-libopus` decoder with bundled libopus. WMA is not supported: Symphonia does not include an ASF demuxer or WMA decoder, so an ASF/WMA file can be identified but cannot be imported. Format support must still be verified on the Windows and Android release builds.

Before inference, Symphonia decodes each source incrementally to temporary 44.1 kHz stereo PCM; it does not retain the full song in RAM. WAV/AIFF/FLAC inputs up to 256 MiB are snapshotted to a private temporary file and read through a memory map; larger inputs and other containers (including AAC, MP3, Ogg/Opus and MP4/WebM) use Symphonia's bounded buffered stream. Source tags are retained as bounded JSON plus indexed SQLite key/value rows; the bitrate field is an average estimate derived from encoded file size and decoded duration (not a guaranteed stream-header bitrate). Streaming EBU R 128 / ITU-R BS.1770 analysis runs on the decoded stereo signal at its source sample rate, before 44.1 kHz resampling and PCM16 conversion: it applies sample-rate-derived K-weighting biquads, 400 ms gating blocks at 75% overlap with the absolute/relative thresholds, and four-times windowed-sinc true-peak estimation. It stores integrated LUFS, dBTP, and the signed gain recommendation to reach −14 LUFS. Playback reads each stored LUFS value and automatically applies per-deck gain compensation toward an adjustable R128 target (−14 LUFS by default, −36 to −6 LUFS from Playback Options) with an 8 ms smooth gain ramp; tracks without a usable loudness measurement receive no compensation. Both native deck renderers plus distinct sample and locally rendered TTS auxiliary buses feed one app-wide MasterBus and one CPAL output stream. The bus sums Deck A, Deck B, Samples, and TTS separately, then applies one shared four-times true-peak, soft-knee lookahead limiter after summation. Its finite lookahead adds latency; the current gain-reduction and latency readings are exposed through Tauri status IPC. The software stage constrains digital samples; it does not guarantee that downstream DACs, drivers, or analog equipment cannot clip. The Rust DSP/MIR worker also estimates BPM/beat-grid and key, and records RMS energy density, dynamic range, onset density, spectral centroid and low/mid/high band ratios. `Aggressiv`, `Deep`, `Conscious`, `Hype`, valence/arousal, mood, rhythm-pattern and style labels are transparent heuristics, not trained affect or genre predictions. Results and a normalized 64-dimensional, signed-int8 handcrafted MIR descriptor are stored by the asynchronous local import worker.

An optional local ONNX projection can be placed at `models/audio-embedding-int8.onnx` under app data. The adapter's fixed contract is float32 `[1, 64]` MIR-feature input and float32 `[1, 64]` output; its normalized output is stored as signed int8. No embedding model is bundled or downloaded, the model's quantized-weight format and license are not verified by the app, and imports transparently fall back to the deterministic MIR descriptor when no compatible local file is present or inference fails. Thus a learned INT8 audio embedding is **not enabled by default** and still requires a suitable, locally supplied and rights-cleared model.

The app retains its user-supplied HT-Demucs ONNX path and adds a CPU-first MDX-Net alternative selected by an explicit `.cvmodel.json` manifest. HT-Demucs graphs are validated against float32 `[1, 2, 343980] → [1, 4, 2, 343980]`; MDX-Net graphs must match float32 `[1, 4, 2048, 512]` and their manifest's source-specific FFT/hop configuration. User-selected MDX files are structurally checked and SHA-pinned in the local manifest before import. CPU is the default, with 1–8 configured threads. On Windows, DirectML is an explicit opt-in and falls back to CPU if session creation fails; Android uses CPU. The two-stem profile predicts vocals and constructs instrumental as input minus vocals. The four-stem profile predicts vocals, Other/instrumental, bass and drums separately; only the selected profile's aligned FLAC files are stored. Four-stem MDX loads one target session at a time to bound memory. Built-in KUIELab A assets can be downloaded on demand from an immutable revision, each with a hard-coded SHA-256 pin, but their model/training-data commercial rights are not cleared. `ort` is configured for dynamic ONNX Runtime loading, so a compatible native runtime and any explicitly requested DirectML dependencies must also be packaged; Windows/Android builds and hardware-performance validation remain release tasks. See [the model registry, exact pins and license gates](docs/model-registry.md).

Lyrics are transcribed locally from the separated acapella with `whisper-rs`. The selected `.bin` file must load as `tiny` or `tiny.en`. Whisper Tiny is an optional pinned registry download; the app does not bundle its weights. Word timestamps are taken from Whisper token timing, with segment-based fallback timing when token timing is unavailable. Text sentiment is a small English/Spanish keyword heuristic with negation/intensifier handling, not a language model or a substitute for human interpretation. The transcript, per-word timing and score remain in SQLite.

On Android, ChampionVibe uses `tauri-plugin-android-fs` and the Storage Access Framework rather than treating `content://` URIs as filesystem paths. Its native picker requests local-only audio files/folders, recursively walks selected folders, and copies selected audio into the app's local `library/sources` directory before analysis. Selected `.onnx` and Whisper `.bin` models are likewise copied under app data. SAF read grants are not persisted across app/device restarts: audio selections are staged when the job starts, so if the app is terminated before then, select them again. This keeps inference and persistent reads on local files and avoids uploading audio; the picker is not configured to select remote/cloud-only entries. The Android OS/provider may still control what it exposes through SAF. The Windows picker returns ordinary filesystem paths; the importer rejects UNC paths and uses the Windows drive-type API to reject mapped network drives and drives whose locality cannot be verified.

Audio, stems, transcripts and inference inputs are not sent to the license issuer or any cloud inference service. Network access is limited to the separately documented payment/license quote and activation flow plus explicit, pinned model downloads from Hugging Face; model downloads contain no user audio. Managed downloads are SHA-256-verified and stored locally. Model/runtime compatibility and the commercial rights to a model and its training data remain separate release gates.

### Separator model licensing blockers

**Do not ship or commercially enable pretrained HT-Demucs weights yet.** The [upstream Demucs maintainer states](https://github.com/facebookresearch/demucs/issues/327) that pretrained weights are not covered by the repository's MIT license and are provided for scientific purposes only. The upstream project says `htdemucs` was trained on MUSDB-HQ plus an additional 800-song dataset; the [official MUSDB18-HQ record](https://zenodo.org/records/3338373) restricts the dataset to educational use absent express permission for commercial use. These sources do not establish commercial rights for the model weights or the extra training data. An ONNX conversion/export repository's MIT label applies to its code/export, not automatically to the source weights or training corpus. Commercial release requires direct written rights covering the exact weights and relevant data, or a replacement model with traceable commercial rights. No pretrained HT-Demucs weights are bundled here.

The KUIELab Track A reference code declares MIT, but that does not grant rights to mirrored MDX-Net weights or training data. The pinned Track A configuration references MUSDB18-HQ; commercial provenance for the exact weights and relevant data is not independently cleared. Those assets are optional, downloaded only on request, and must not be marketed as commercially cleared.

## Native audio engine

The native output and DSP path is Rust-only: CPAL (Apache-2.0) selects WASAPI on Windows and AAudio on Android; Symphonia (MPL-2.0) streams supported formats on background workers, with bundled `libopus` used only through the Opus adapter; `rtrb` (MIT/Apache-2.0) provides bounded lock-free SPSC queues. These are open-source libraries without per-install royalties. Android uses API 26+ for AAudio. Windows Media Audio/ASF remains unsupported.

Imported tracks use SQLite stem kinds `acapella`, `instrumental`, `bass` and `drums`. Four-stem files share the same start point and arrangement timeline; the engine preserves frame alignment but does not infer or correct editorial offsets. `load_track_audio(trackId)` validates the active lanes and prebuffers them off the audio callback, then exposes `play_audio`, `pause_audio`, `stop_audio`, `get_audio_status` and `set_audio_controls`. Four-stem tracks use all four lanes on Windows and Pro; a two-stem track selects only acapella + instrumental even on a Pro device. Android Demo likewise requests two lanes. Inactive bass/drums workers are not started and the callback loops only over the active lanes. If a two-stem track has no bass/drums rows, the shared four-lane data structure aliases those unused paths to the instrumental without opening them. Sources with differing sample rates are linearly resampled on their decoder threads to the per-deck rate. The optional DJ mix engine retains two independent logical deck renderers so outgoing and incoming tracks can overlap, but they do not create independent hardware streams. Each deck applies its own stem gains, high-/low-pass biquads, three-band isolator with kill switches, modulated tape delay, damped spring-style reverb, and soft output saturation; automatic R128 compensation is applied per deck from stored integrated LUFS toward the adjustable Playback Options target (default −14 LUFS, UI range −36 to −6) with an eight-millisecond gain ramp. Quantized samples and per-deck local TTS shoutouts use distinct deck auxiliary buses and bypass WSOLA/varispeed. Kokoro PCM can also enter an independent, global master TTS Aux queue; that signal is resampled to the CPAL rate and scheduled against the active deck's beat/bar grid. Both deck outputs and all auxiliary buses enter the single shared MasterBus/CPAL stream for final summation and true-peak limiting. Limiter lookahead is finite and adds latency; both latency and measured gain reduction are exposed in audio status.

The playback callback does no disk I/O, decoding, mutex locking or heap allocation. It uses a preallocated DSP graph, atomics and four bounded SPSC stem rings; the rings hold about 1.36 seconds per active stem and the engine primes about 170 ms before playback. On a short underrun it emits silence without advancing any active stem consumer, preserving inter-stem alignment, and increments the xrun counter. The shared MasterBus sends a single stereo capture tap after Deck A/B, sample, per-deck TTS, and global master TTS summation plus the live master limiter through a bounded SPSC ring to a background recording worker. The worker preserves that post-master signal; it does not re-sum the decks or apply a second recording-only limiter. The Rust writer has concrete paths for WAV PCM 16/24-bit, FLAC PCM 16-bit, MP3 320 kbps, OGG/Vorbis VBR, and M4A/AAC 160 kbps, and the IPC format inventory drives the selector. The MP3, Vorbis, and AAC/MP4 crate integrations and their signature/container tests are present in source, but Rust builds/tests and Windows/Android target compatibility still require verification. A full capture ring aborts and deletes the incomplete file rather than blocking an audio callback. This is designed for low latency, not physically zero latency; device/driver scheduling and storage still affect timing. Windows recordings go under `%USERPROFILE%/Music/ChampionVibe/Recordings/`; Android uses the app-scoped `getExternalFilesDir`.

Example Rust command calls from Tauri IPC:

```js
await invoke('load_track_audio', { trackId: 42 });
await invoke('play_audio');
await invoke('set_audio_controls', { update: {
  stemGains: [1.0, 0.8, 1.0, 1.0],
  killBass: false,
  tapeMix: 0.15,
  springMix: 0.1
} });
```

There is no Web Audio API use. The frontend can only request backend operations; the audio callback and sample processing remain native.

### Player, waveform IPC and performance UI

The default view is a compact local player with track information, play/pause/stop/next, a progress display, per-stem mute switches, Auto-Gain enable/Off/−14/−16 LUFS controls applied to both native decks, and a track-level estimated dB offset from integrated LUFS. It also exposes a Pitch-Lock/WSOLA switch; disabling it selects local varispeed, which changes pitch with tempo. For an already-playing tempo-shifted deck, both branches are rendered and the shared output callback crossfades between them over 10 ms without reloading the track. A separate touch-first Performance Mode exposes Force Next, Pull Up/Rewind, sample drops, bass/mid/high kill switches and optional local DJ shoutouts. Library search is debounced and paged; natural-language terms can filter local mood, rhythm, musical key, BPM and integrated loudness. The virtual list recycles at most 15 row nodes rather than creating one per track and shows localized, color-coded mood/rhythm badges. Each row also shows a format badge derived from the decoder-reported codec when available, using the file extension only as a fallback.

The Vanilla frontend uses the static `de.json`, `en.json`, `es.json`, and `fr.json` dictionaries with no runtime i18n dependency. It chooses the first supported browser locale on first run and switches synchronously without reloading. The language selector and all translated static and runtime UI—including tooltips, errors, help, legal text, and licensing dialogs—share those dictionaries.

On desktop, Canvas waveform generation is requested only after Play and only for the current track. Android disables Canvas and offers a simple progress bar or no visualization; the progress bar uses a lightweight animation while playing, and the off setting stops that visual ticker. Peaks and beat-grid markers cross Tauri IPC as a compact little-endian `CVW1` binary `Response` (ArrayBuffer on the WebView side), not large JSON arrays. The packet contains a 16-byte header, u16 peak magnitudes, and u32 beat times in milliseconds.

The unlicensed Demo badge remains visible; Windows Demo uses four active stems and Android Demo uses two. Demo Record opens the license paywall and Demo does not persist a listening-session snapshot. After issuer-side on-chain verification, Pro removes the Demo badge, enables the Rust-side recording entitlement check, and stores a small local session snapshot. Master recording is Pro-only: Rust validates both `record` and `save`, arms a monotonic expiry deadline in the capture flags, and the writer deletes a partial recording on expiry or ring overflow. WAV PCM 16/24-bit, FLAC PCM 16-bit, MP3 320 kbps, OGG/Vorbis VBR, and M4A/AAC 160 kbps each map to a concrete Rust writer branch and are enabled in the selector. Encoder output tests are in source, but Rust execution and release-target compatibility have not yet been confirmed in this environment. Help/FAQ, terms/audio disclaimer, and AndrexTheDev developer contact are available in the info dialog.

## Automatic DJ mix engine and Freestyle Maestro

The Rust decision/state machine lives in `src-tauri/src/mix_engine.rs`; it reads sentiment, acoustic tags, BPM/key, local stem paths, beat grids, riddim-family/manual tags, and Whisper word timestamps from SQLite. Tauri commands `start_mix_engine`, `set_mix_engine_preset`, `get_mix_engine_status` and `stop_mix_engine` expose it to the lightweight preset selector. The runner preloads the next local track on the idle logical deck and starts only the active profile's decoder workers (two lanes for Android Demo, four for Windows/Pro). Both logical decks feed the same app-wide MasterBus and one native CPAL output stream, which mixes them on a shared device clock before the live limiter; sample-aligned output behavior still requires Windows/Android hardware testing.

The full routing tree is deterministic:

```text
P1 Smooth Selector ────────────────────────> Deep/Roots policy
P2 Club/Hype ──────────────────────────────> Club/Hype policy
P3 Aggressiv/Clash ────────────────────────> Aggressiv/Clash policy
P4 Freestyle Maestro:
  current acoustic tag Aggressiv ──────────> Aggressiv/Clash
  else current acoustic tag Hype ──────────> Club/Hype
  else current acoustic tag Deep/Conscious > Deep/Roots
  else use incoming acoustic tag if present
  else manual tags: Clash/Aggressive/War > Hype/Club/Dance/Party > Deep/Roots/Conscious/Dub
  else weighted sentiment (current 0.65, incoming 0.35):
       score <= -0.35 ─────────────────────> Aggressiv/Clash
       score >= +0.30 and max energy >= 0.42 > Club/Hype
  else current negative label ─────────────> Aggressiv/Clash
  else current positive label and energy >= 0.42 > Club/Hype
  else current energy <= 0.36 ────────────> Deep/Roots
  else retain previous style; first unknown track defaults to Deep/Roots
```

Transition policies:

- **Deep/Roots:** a four-, six-, or eight-bar equal-power crossfade over two preloaded decks is selected from local mood, rhythm, key, loudness and BPM compatibility. The incoming curve and bass hand-off timing are shaped by that MIR score; outgoing spring reverb and beat-synced tape delay pulse as local Auto-Dub. The start is snapped back to a bar in the saved beat grid. No cloud service is used.
- **Club/Hype:** a hard cut at the next detected chorus in the outgoing track; incoming starts at its next chorus cue when available. Chorus sections are inferred locally from repeated four-word Whisper n-grams separated by at least 12 seconds (there is no explicit chorus column yet). Candidate tracks from the same computed/manual riddim family are mandatory when any playable match exists; otherwise the selector falls back to tempo/key-compatible local stems and reports that no family match was found. With no inferred chorus, it cuts one bar before the known duration.
- **Aggressiv/Clash:** the last meaningful Whisper words of the current track are looked up through the indexed local lyric-word table. A candidate's timestamped phrase with the most distinct shared words becomes the incoming cue; if no word match exists, tempo/key scoring and then chorus/track-start cues are used. Auto-Rewind is armed from playback start; near the last bar the bass/mid/high kill switches engage, and the runner begins its short crossfade when the track wraps through the native vinyl-rewind effect. Unknown duration still waits for the actual end/rewind.

Tempo matching compares half-, normal-, and double-time beat interpretations of the incoming BPM against the outgoing effective BPM, chooses the smallest relative error (preferring normal speed on ties), and stores the interpretation in `MixDecision`. `playbackRate` is applied before prebuffering for rates from 0.80 to 1.20. With Pitch-Lock enabled, a streaming pure-Rust WSOLA processor lets the instrumental stem choose analysis offsets and applies the same schedule to all active stems, maintaining their transient alignment while largely preserving pitch. With Pitch-Lock disabled, each aligned stem uses bounded streaming varispeed and pitch changes with tempo. The live Pitch-Lock choice is sampled by the shared output callback for each audio frame; changing the toggle crossfades between the already-rendered WSOLA and varispeed branches over 10 ms, without a deck reload. Half/double-time are beat-grid interpretations; actual stretch remains within the ±20% limit. Semantic routing is deterministic local word overlap, not a cloud LLM or an embedding model. An unavailable/invalid local path or a library with no second playable track for the active stem profile is surfaced as a mix error; the local audio engine continues to report its own device and underrun state.

The automatic mixer keeps two logical deck renderers but opens only one shared native output stream; per-deck gain ramps are rendered into bounded SPSC rings and mixed on the CPAL device clock. This removes cross-stream clock drift in the application graph, but output-device latency and actual Windows WASAPI/Android AAudio behavior still need hardware validation. The callback remains allocation-free and local; no Web Audio API is introduced.

## Local riddim families and sound-system tools

`riddim.rs` fingerprints only the instrumental stem. It decodes at 11.025 kHz for up to 90 seconds and stores a compact 272-value spectral/time descriptor, a 64-bit SimHash, an exact descriptor hash, and eight indexed LSH bands in SQLite. Rebuild compares only LSH candidates rather than all track pairs and cheaply rejects candidates with more than 24 differing SimHash bits before cosine scoring. Exact/strong instrumental similarity can form a family on its own; weaker audio similarity needs at least 0.50 Jaccard overlap among manually assigned tags. Matching explicit `riddim` tags also connect tracks, including tracks without a usable instrumental. Tags, descriptors and family links stay in the local database. Fingerprint versioning lets future algorithm changes re-index old instrumental stems.

The sample bank starts with locally synthesized siren, airhorn and vinyl-rewind FX. It accepts mono/stereo PCM WAVs (8–32-bit integer or 32-bit float, up to 192 kHz), loads and resamples samples in Rust, and caps the bank at 64 clips total, 30 seconds per user clip and 128 MiB of decoded sample RAM. Windows can select a folder; Android uses a local-only Storage Access Framework folder picker and stages up to 61 WAVs under app data before loading them. Pads can trigger immediately or at the next beat/bar on the loaded track's grid. Wheel and Come Again reopens the retained four stem files at frame zero, plays the built-in rewind FX and optional selected sample, then resumes at beat one; auto-rewind can apply at track end. Device errors and underruns are exposed by `get_audio_status`.

Dubplates can use optional `piper-rs` CPU synthesis with user-selected local `.onnx` and `.onnx.json` files; no audio is sent to a service. Phrase templates add selector/riddim/wheel calls around custom text, and the selected track's local mood label maps to a text/punctuation cue (not model-native emotion control). The generated voice's median fundamental pitch is shifted by the shortest semitone interval to the selected track key, statically leveled toward the −14 LUFS master target with a conservative −1.5 dBTP ceiling, then loaded into the RAM sample bank for grid quantization. This lightweight resampling changes voice duration and timbre; it is not formant-preserving and key matching is an approximation, not musical accompaniment. This optional code path and its models still require a Rust build, runtime, and licensing validation before release.

The optional Kokoro path downloads the pinned ONNX model and `af_heart` voice only after the user chooses them in the local model library; Rust verifies each artifact's SHA-256 before returning binary data to ONNX Runtime WebAssembly. The lightweight `src/services/patois_g2p.js` applies the listed, word-bounded spelling substitutions before local English IPA G2P. These are approximate pronunciation cues, not a complete dialect model or a claim of authentic Jamaican Patois. Hype/aggressive profiles provide rate, semitone-pitch and phrase-final contour vectors: rate is applied through Kokoro's speed input, while a small local linear-interpolation varispeed pass shapes pitch/intonation with approximate rate compensation. That inexpensive pass can alter timbre; it is not formant-preserving and does not add model-native emotion control. Synthesis remains local and produces 24 kHz PCM. `push_tts_audio_buffer` sends float32 little-endian PCM to Rust (raw `ArrayBuffer` Tauri IPC on Windows); Rust validates it, resamples it to the CPAL device rate and schedules it on the next selected beat or bar of the active deck's grid. It enters the standalone master TTS Aux before the shared limiter, so the same post-limiter signal is present in master recordings. **Tauri v2 does not support raw request bodies on Android, so the Android build uses the approved compact base64-string transport, never JSON number arrays.** Kokoro's current `af_heart` voice is US English, not Jamaican Patois, and this path does not match the voice to the track key.

**TTS release blocker:** the default build deliberately leaves `local-piper-tts` disabled. The [currently maintained Piper repository declares GPL-3.0](https://github.com/OHF-Voice/piper1-gpl/blob/main/COPYING), and [eSpeak NG also declares GPL-3.0](https://github.com/espeak-ng/espeak-ng/blob/master/COPYING); the Rust adapter's package metadata alone does not settle the license of upstream code built or linked by its dependency tree. The exact engine, dependency and voice-model bill of materials needs review before distribution. Piper's [official voice guide](https://github.com/OHF-Voice/piper1-gpl/blob/main/docs/VOICES.md) says each voice's `MODEL_CARD` contains important licensing terms and warns that some voices are restrictive; no Jamaican Patois voice has been vetted for this feature. The Kokoro repository declares Apache-2.0, but voice-data provenance and redistribution terms still need review. GitHub Actions now compiles/packages Windows and arm64 Android builds, but real-model loading, WebAssembly inference, and end-to-end TTS playback through the master audio bus still require Windows 10 and Android device testing. Do not bundle or enable an engine or voice for commercial distribution until licensing and platform compatibility are cleared. The same caution applies to HT-Demucs weights described above.

## License and issuer setup

The Tauri binary contains **only a public Ed25519 verification key**. The signing seed belongs only on the issuer host. To create a keypair seed, keep a 32-byte random hex seed in a secret manager (or generate one locally with a cryptographically secure generator). Do not commit it or paste it into the app.

Print the matching public key on the issuer machine:

```sh
CHAMPIONVIBE_SIGNING_KEY_HEX='<32-byte-seed-as-hex>' \
  cargo run -p championvibe-issuer -- --print-public-key
```

Start the issuer (default: loopback-only on `127.0.0.1:8080`):

```sh
CHAMPIONVIBE_SIGNING_KEY_HEX='<32-byte-seed-as-hex>' \
  cargo run -p championvibe-issuer
```

Use that public key when building the client, so it can verify licenses:

```sh
CHAMPIONVIBE_LICENSE_PUBLIC_KEY_HEX='<printed-public-key>' npm run tauri build
```

On Windows PowerShell, set the same values with `$env:CHAMPIONVIBE_SIGNING_KEY_HEX = '...'` and `$env:CHAMPIONVIBE_LICENSE_PUBLIC_KEY_HEX = '...'` before the respective command. If no verification key was embedded at build time, the app fails closed in demo mode.

The default issuer bind is local-only. For a remote issuer, use HTTPS at a reverse proxy and set `CHAMPIONVIBE_ISSUER_BIND` only behind appropriate TLS, firewall, rate-limit and abuse controls. The app accepts remote issuer URLs only over HTTPS; plain HTTP is allowed only for localhost. The browser UI stores the issuer URL locally and sends it to Rust, which makes the request.

Issuer variables:

| Variable | Purpose | Default |
| --- | --- | --- |
| `CHAMPIONVIBE_SIGNING_KEY_HEX` | 32-byte Ed25519 seed; issuer only | required |
| `CHAMPIONVIBE_ISSUER_BIND` | HTTP listen address | `127.0.0.1:8080` |
| `CHAMPIONVIBE_ISSUER_DB` | Issuer quote/replay/subscription DB | `issuer-data/licenses.sqlite` |
| `CHAMPIONVIBE_SOLANA_RECEIVER` | Solana treasury address | `79KsqtJJdhKFJ9woxnYgtf3nq7HxQveafWBCtC3mxWi8` |
| `CHAMPIONVIBE_BITCOIN_RECEIVER` | Bitcoin mainnet address | `bc1qeqzrlfg3edrydk4s0hecakc82gp26n5p7hkc7f` |
| `CHAMPIONVIBE_ETHEREUM_RECEIVER` | Ethereum mainnet address | `0xBC3fab34f69bc9f6661608C3FB36dDdC313C42F7` |
| `CHAMPIONVIBE_SOLANA_RPC` | Solana JSON-RPC | `https://api.mainnet-beta.solana.com` |
| `CHAMPIONVIBE_BITCOIN_API` | Bitcoin explorer REST API | `https://blockstream.info/api` |
| `CHAMPIONVIBE_ETHEREUM_RPC` | Ethereum JSON-RPC | `https://cloudflare-eth.com` |
| `CHAMPIONVIBE_PRICE_ORACLE_RPC` | Ethereum JSON-RPC used to read on-chain Chainlink feeds | `https://cloudflare-eth.com` |

A quote is issuer-stored, reads fresh on-chain Chainlink crypto/USD and EUR/USD feeds over the free Ethereum JSON-RPC endpoint, and expires for new payments after 15 minutes. The feed round timestamp and precision are checked; no Pyth/Hermes API key or paid price API is used. Payment verification accepts **native SOL, BTC, or ETH only**: finalized Solana direct System Program transfers, Bitcoin outputs after six confirmations, and finalized Ethereum native-value transfers. SPL tokens, ERC-20, Lightning and wrapped assets are not accepted. The amount must meet or exceed the quoted minimum; overpayments are not automatically refunded. Quote settlement is permitted for a limited confirmation window.

The issuer checks the chain and quote, prevents a transaction hash from being replayed, extends a subscription by one calendar month or year, and signs the resulting entitlement. Wallet address connection is informational only; it is never treated as proof of payment. The UI checks browser-injected wallet providers when present. Tauri WebViews do not guarantee injected wallet support, so the external-wallet + pasted-TxHash flow remains available.

## Device binding and enforcement limits

- Windows queries the motherboard serial (`Win32_BaseBoard.SerialNumber`) and CPU `ProcessorId` through fixed PowerShell/CIM queries; placeholder firmware values fail closed.
- Android reads `Settings.Secure.ANDROID_ID` in a Kotlin Tauri mobile plugin and invokes it from Rust via `run_mobile_plugin`.
- Linux has a development-only `/etc/machine-id` fallback.
- Rust hashes the identifiers with a ChampionVibe-specific domain separator. Raw values are neither persisted nor transmitted; only the digest is bound into a signed license.

This is device binding, not hardware-backed attestation: some firmware does not expose unique board serials, CPUID-derived ProcessorId values may not be globally unique, hardware replacement changes the digest, Android ID is scoped by platform rules, and a compromised client can spoof the result. The Rust gate checks the signature, binding and exact expiry with zero grace before authorization, before arming master capture, and again before saving recording metadata. The audio callback is armed only until the signed expiry deadline; a desktop clock can still be tampered with offline, so high-assurance expiry requires periodic issuer/server time or another trusted execution mechanism.

## Free services and trust assumptions

No paid API or WalletConnect project ID is required by this scaffold. It reads Chainlink price-feed contracts through a public Ethereum RPC and verifies payments through public chain endpoints. Those community RPCs may throttle, change terms or go offline; use redundant providers or self-hosted nodes for production availability. The issuer itself needs a host and secure key backup. A “free forever” operational guarantee cannot be made for third-party endpoints, hardware or hosting.

## CI builds and downloads

`.github/workflows/ci.yml` starts on every push and pull request and can also be run manually. It packages Windows `.msi`/`.exe` and Android `.apk` files as GitHub Actions artifacts (14-day retention). The current workflow does not publish GitHub Release assets. Use `scripts/download-actions-builds.sh <run-id>` or see [the artifact download guide](docs/github-actions-artifacts.md) for latest-run, run-ID and Release downloads.

## Validation

The frontend includes Node's built-in packet-decoder tests for binary waveform headers, endianness, view offsets, truncation and payload limits; run the frontend checks with:

```sh
npm test
npm run build
```

The repository also includes Rust unit tests for SQLite operations and paging, stem-profile validation, license verification, price calculations and transaction parsing. Run them with:

```sh
cargo test --workspace
cargo fmt --all -- --check
```

Rust build/test commands require the Rust toolchain and native platform prerequisites installed. The real-time output path must additionally be soak-tested on Windows WASAPI and Android AAudio hardware, since emulators and preview builds cannot certify underrun-free behavior.
