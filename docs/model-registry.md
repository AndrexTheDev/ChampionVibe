# Model registry, cleared separator default, and BYOM policy

ChampionVibe ships **no** model weights. There are exactly two ways a separator can reach the app:

1. **Cleared default (managed download).** `src-tauri/assets/model-registry.json` lists only artifacts
   whose code *and* weights are usable commercially. The standard separator is the **Spleeter 2-stem
   ONNX** export (MIT Spleeter weights by Deezer Research; Apache-2.0 ONNX conversion by
   [`k2-fsa/sherpa-onnx`](https://github.com/k2-fsa/sherpa-onnx)). A download is an explicit user
   action over HTTPS, is size-limited, hashed while on disk, checked against the registry byte size,
   and only then installed atomically. A pin that has not been verified against the upstream bytes
   (`PENDING_VERIFICATION`) makes the artifact **non-installable** instead of silently trusted.
2. **BYOM (Bring Your Own Model).** The user drops a local `.onnx`/`.cvmodel.json` file. ChampionVibe
   never downloads, copies, bundles or redistributes those weights; it validates tensor shapes,
   stores the local path plus the file hash in `byom-models.json`, and requires an explicit rights
   acknowledgement for kinds whose provenance is unclear.

Every unclear-rights separator source (HT-Demucs/Demucs mirrors, KUIELab/UVR MDX mirrors, Open-Unmix)
is listed in `policy.forbiddenDownloadSources`; `scripts/verify-model-pins.sh` and the Rust unit tests
fail if any registry URL resolves to one of them.

## The standard separator: Spleeter 2-stem ONNX

| Registry ID | Asset | SHA-256 | Bytes |
| --- | --- | --- | --- |
| `spleeter-2stems-onnx` | `sherpa-onnx-spleeter-2stems.tar.bz2` | `e26401d9c1801f43c0229731d78d32c2e80085e3aceedeb25f29d2de5fa68ca2` | 74,682,545 |
| `spleeter-2stems-onnx-fp16` | `sherpa-onnx-spleeter-2stems-fp16.tar.bz2` | `d54561979bd2e08a51e7dbd99ac36bb47564e089eefd403636dbca93e811bba2` | 35,271,738 |

Base URL: `https://github.com/k2-fsa/sherpa-onnx/releases/download/source-separation-models/`.
Each archive contains `vocals.onnx` and `accompaniment.onnx`. Both pins were produced by a byte-exact
download in the `Model pins and lockfile` GitHub Actions workflow and are recorded, with timestamp and
job reference, in `pinVerifiedAt`/`pinVerification` inside the registry plus
`reports/model-pin-verification.json`.

### Spleeter 2-stem runtime contract

The Rust backend (`src-tauri/src/import/spleeter.rs`) mirrors the reference implementation in
sherpa-onnx's `offline-source-separation-spleeter-impl.h`:

| Parameter | Value |
| --- | ---: |
| Sample rate | 44,100 Hz (stereo) |
| FFT size / window | 4,096 samples, periodic Hann |
| Hop length | 1,024 samples |
| `center` | `false` |
| Bins fed to the model | 1,024 of 2,049 |
| Model input / output | float32 `[2, splits, 512, 1024]`, ONNX names `x` → `y`, dynamic axis 1 |
| Mask | `(stem² + ε/2) / (vocals² + accompaniment² + ε)`, `ε = 1e-10` |
| Stems | stem 0 → `acapella`, stem 1 → `instrumental` |

Processing streams in 512-frame chunks with weighted overlap-add; the `center = false` framing cannot
cover the final `n_fft - hop` samples, so that tail is padded with silence to keep the stem timeline
identical to the source. Note that the exporter's ONNX metadata contains `sample_rate: 41000` (a typo
in the upstream export script); the real contract is 44.1 kHz and the app ignores that metadata field.

## Cleared managed artifacts (non-separator)

| Registry ID / file | Source revision | SHA-256 | Use |
| --- | --- | --- | --- |
| `whisper-tiny` · `ggml-tiny.bin` | `ggerganov/whisper.cpp@80da2d8bfee42b0e836fc3a9890373e5defc00a6` | `be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21` | Local Whisper Tiny transcription (MIT) |
| `kokoro-model` · `model_quantized.onnx` | `onnx-community/Kokoro-82M-v1.0-ONNX@468588286ebb2dd77c25b9771e5d165896538cce` | `fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478` | Optional local Kokoro WebAssembly TTS (Apache-2.0 declared) |
| `kokoro-voice-af-heart` · `af_heart.bin` | Same revision · `voices/af_heart.bin` | `d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b` | Optional voice vector for **US English** synthesis; not Jamaican Patois |

## BYOM kinds

| Kind | Accepted file | Contract | Acknowledgement |
| --- | --- | --- | --- |
| `spleeter-2stems` | `.cvmodel.json` Spleeter descriptor (two graph paths + hashes) | float32 `[2, splits, 512, 1024]` | not required (MIT) |
| `htdemucs` | `.onnx` | float32 `[1, 2, 343980] → [1, 4, 2, 343980]` | required; Demucs maintainers state the pretrained weights are not MIT |
| `mdx-net` | `.cvmodel.json` MDX manifest | float32 `[1, 4, 2048, 512]` per target | required; KUIELab/UVR mirrors have unclear model and training-data provenance |

Records live in `<app data>/models/byom-models.json` and contain the absolute local path, file name,
byte size, SHA-256, the validated contract, and the acknowledgement flag — never a copy of the weights.
The UI shows file name, size, SHA-256 and licence context, and states that the file stays local and is
never uploaded.

### MDX-Net manifest contract (BYOM detail)

A ChampionVibe `.cvmodel.json` manifest pins every file and declares the target stem, FFT size,
1,024-sample hop, 2,048 retained frequency bins, 512 STFT frames, and amplitude compensation. The
KUIELab Track A configuration is:

| Target | `n_fft` | ChampionVibe lane |
| --- | ---: | --- |
| vocals | 6,144 | acapella |
| other | 8,192 | instrumental |
| bass | 16,384 | bass |
| drums | 4,096 | drums |

The ONNX graph must accept and emit float32 `[1, 4, 2048, 512]` (left-real, left-imag, right-real,
right-imag). Two-stem profiles predict vocals and build the instrumental as mixture minus vocals;
four-stem profiles predict all four targets, and `Other` maps to the instrumental lane. Two-stem
Spleeter and MDX tracks are stored with only their actual output paths, and `stem_paths[0]` is always
the vocals stem so Whisper always transcribes an acapella.

## Pin verification

- `scripts/verify-model-pins.sh` is the strict verifier. It rejects forbidden sources, checks byte
  size and SHA-256, cross-checks the upstream `checksumSource` when it lists the artifact, and writes
  the `championvibe-model-pin-verification-v1` report. `--allow-pending` downgrades unverified pins to
  warnings for discovery runs.
- `.github/workflows/model-pins.yml` runs it on every push, commits the report back to the branch, and
  generates/commits `Cargo.lock` so releases build from a locked dependency graph.
- `reports/model-pin-verification.json` is the machine-readable evidence for the current pins.
- `scripts/verify-cleared-model-policy.sh` is the offline gate that runs on every push in the
  `compliance` CI job and inside `npm test`: it checks the tiers, pins, forbidden sources, bundled
  weights, BYOM acknowledgement rules, the CC0 sampleboard bytes, the locale transparency wording and
  the `--locked` build wiring without downloading anything.
- `reports/rust-diagnostics.txt` is not part of the pipeline; the temporary diagnostics workflow that
  produced it was retired once CI started checking `--all-targets` and failing on warnings itself.

## Licensing gates

- **Spleeter (default separator).** The Deezer Spleeter repository declares MIT for its code, and the
  JOSS paper (JOSS 5(50):2154, 2020) states that the *source code and pre-trained models* are
  distributed under an MIT license. The ONNX conversion tooling in sherpa-onnx is Apache-2.0; the
  upstream export pipeline (freeze → torch → ONNX) is traceable to `deezer/spleeter` v1.4.0
  `2stems.tar.gz`. That traceability plus the byte-exact SHA-256 pin is why this profile is the
  cleared default.
- **HT-Demucs.** The maintainers state the pretrained weights are not covered by the repository's MIT
  license and were trained on MUSDB-HQ plus additional data; MUSDB18-HQ restricts commercial use.
  ChampionVibe therefore ships no HT-Demucs download path; the tensor contract stays supported only
  for user-supplied BYOM files, and the UI requires an explicit rights acknowledgement.
- **MDX-Net (KUIELab Track A / UVR mirrors).** The reference code is MIT, but that does not grant
  rights to mirrored weights or training data. No download path exists; BYOM only, acknowledgement
  required, and the app must not be marketed as commercially cleared for these files.
- **Open-Unmix.** No download path, no code path, no references.
- **Whisper.** OpenAI Whisper and `whisper.cpp` declare MIT; retain their notices when redistributing.
- **Kokoro.** The pinned model repository declares Apache-2.0, but runtime, ONNX conversion, voice
  pack, tokenizer, phonemizer, G2P and pronunciation-dictionary data each need separate review. The
  `af_heart` voice is US English; the optional Patois spelling substitutions are an approximation, not
  a dialect model. Do not claim an authentic Patois voice.
- **Piper (optional `local-piper-tts` feature).** Gated pending runtime/eSpeak and per-voice
  redistribution review; do not enable it in a release without completing that review.

## External references

- [Spleeter (Deezer Research) — MIT code and pretrained models](https://github.com/deezer/spleeter)
- [Spleeter JOSS paper — MIT statement for code and models](https://joss.theoj.org/papers/10.21105/joss.02154)
- [sherpa-onnx source-separation model releases](https://github.com/k2-fsa/sherpa-onnx/releases/tag/source-separation-models)
- [sherpa-onnx Spleeter C++ reference implementation](https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/offline-source-separation-spleeter-impl.h)
- [Pinned Whisper Tiny model commit](https://huggingface.co/ggerganov/whisper.cpp/commit/80da2d8bfee42b0e836fc3a9890373e5defc00a6)
- [Pinned Kokoro model commit](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/commit/468588286ebb2dd77c25b9771e5d165896538cce)
- [HT-Demucs weights/licence statement](https://github.com/facebookresearch/demucs/issues/327)
