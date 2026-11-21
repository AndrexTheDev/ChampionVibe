# Third-party notices

ChampionVibe bundles no model weights. This file lists the third-party components and media assets
that are either compiled/embedded into the app or downloaded on explicit user request, together with
their licence and the obligations that follow from it.

## Embedded in the repository and build

| Component | Version / source | Licence | Notes |
| --- | --- | --- | --- |
| Patois/reggae drop sampleboard (`src-tauri/assets/samples/*.wav`) | Original synthesis by this project | CC0-1.0 (public domain dedication) | No attribution required; see `src-tauri/assets/samples/README.md` for per-file hashes |
| Rust crate dependencies (incl. `ort` bindings, `whisper-rs`, `symphonia`, `cpal`, `rusqlite`, `rustfft`, `flacenc`, `hound`, `bzip2`, `tar`) | `Cargo.lock` | MIT / Apache-2.0 / MPL-2.0 / BSD-3-Clause, per crate | `Cargo.lock` is the authoritative inventory; regenerate with the `Model pins and lockfile` workflow |
| `onnxruntime-web` (WebAssembly TTS) | npm `onnxruntime-web` | MIT | Only the downloaded runtime is loaded into the WebView |
| `phonemize` (English G2P) | npm `phonemize` | MIT | Used for the English phonemisation behind the Kokoro path |

## Native runtime bundled by the release workflow

| Component | Source | Notices and release considerations |
| --- | --- | --- |
| ONNX Runtime with DirectML (Windows x64) | Microsoft's [ONNX Runtime DirectML 1.24.3](https://pypi.org/project/onnxruntime-directml/1.24.3/#files), pinned by SHA-256 | The installer copies `LICENSE` and `ThirdPartyNotices.txt` from the same wheel as the DLLs into `licenses/`. Review any separate terms for the companion binary components before redistribution. |
| ONNX Runtime (Android arm64) | Microsoft's `com.microsoft.onnxruntime:onnxruntime-android:1.24.3` AAR on Maven Central | The archive and library are pinned. The APK includes the upstream 1.24.3 `LICENSE` and `ThirdPartyNotices.txt` under `assets/championvibe-licenses/onnxruntime/`; verify any additional Android-specific obligations before public distribution. |

See [native runtime packaging](docs/native-onnx-runtime.md) for verification details. Native runtime libraries are not model weights.

## Downloaded only on explicit user request (SHA-256 pinned)

| Artifact | Source | Licence and obligations |
| --- | --- | --- |
| `sherpa-onnx-spleeter-2stems[-fp16].tar.bz2` (`vocals.onnx`, `accompaniment.onnx`) | `github.com/k2-fsa/sherpa-onnx` release `source-separation-models` | Spleeter weights © Deezer Research, MIT (the JOSS paper states the source code *and pre-trained models* are MIT). ONNX conversion tooling Apache-2.0 (`k2-fsa/sherpa-onnx`). Pins and provenance: `docs/model-registry.md` |
| `ggml-tiny.bin` (Whisper Tiny) | `ggerganov/whisper.cpp` (pinned revision) | OpenAI Whisper / `whisper.cpp` declare MIT. Retain notices when redistributing |
| `model_quantized.onnx`, `af_heart.bin` (Kokoro) | `onnx-community/Kokoro-82M-v1.0-ONNX` (pinned revision) | Repository declares Apache-2.0. Voice-data provenance, phonemiser and G2P dictionary data require separate review before commercial redistribution |
| `audio-embedding-int8.onnx` (optional local MIR projection) | User-supplied | Not bundled or downloaded by ChampionVibe; the user carries the rights |

## Deliberately not downloadable

HT-Demucs / Demucs pretrained weights, KUIELab/UVR MDX-Net mirrors and Open-Unmix are **not**
downloadable from ChampionVibe. Their code or weight rights are not cleared for commercial use (see
`docs/model-registry.md`, "Licensing gates"). They can only be supplied as local BYOM files, where the
user confirms responsibility for the weights. No Open-Unmix code or reference exists in this
repository.

## Optional, gated features

The `local-piper-tts` Cargo feature stays disabled by default: Piper and eSpeak NG are GPL-3.0 and no
per-voice redistribution review has been completed. Do not enable or ship this feature without
finishing that review.
