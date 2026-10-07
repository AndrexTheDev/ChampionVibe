# Build verification

What CI proves about a ChampionVibe commit, and what it cannot prove yet.

## Jobs

| Workflow / job | Runs | Proves |
| --- | --- | --- |
| `ci.yml` · `frontend` | `npm ci`, `npm run build`, `node --test` | The WebView build compiles and all frontend suites pass, including the model-library, i18n and BYOM contract tests |
| `ci.yml` · `rust` | `cargo fmt --all -- --check`, `cargo check --workspace`, `cargo test --workspace` | Formatting, compilation of the full workspace on Linux with Tauri/audio system dependencies, and all Rust unit tests (`model_loader`, `samples`, Spleeter frame/STFT bookkeeping) |
| `ci.yml` · `package` | Tauri bundling on `windows-latest` and Android `aarch64-linux-android` | Windows MSI/NSIS and Android APK artifacts (`championvibe-windows-test-build`, `championvibe-android-apk`, 14-day retention) build from the same commit |
| `model-pins.yml` · `lockfile` | `cargo generate-lockfile`, `::notice` annotation, commit-back | `Cargo.lock` tracks the full workspace dependency graph and is committed to the branch; the job reports the lock SHA-256 and package count |
| `model-pins.yml` · `model-pins` | `scripts/verify-model-pins.sh` + forbidden-source check | Every downloadable artifact matches its registry byte size and SHA-256, no forbidden separator source is reachable, and `reports/model-pin-verification.json` is refreshed |

## What the pin job actually establishes

The strict verifier downloads each artifact from its pinned URL and fails on size drift, hash drift, a
reachable forbidden source, or a pin that is still `PENDING_VERIFICATION`. When the upstream release
publishes a checksum file that lists the artifact, that value is cross-checked too; for the
`sherpa-onnx-spleeter-2stems*` archives k2-fsa's `checksum.txt` does not list them, so those pins rest
on the byte-exact CI download plus the recorded size, timestamp and run reference.

## Warning-free workspace

The Rust job runs `cargo fmt --all -- --check` and
`cargo check --workspace --all-targets --message-format=short`, and it **fails on any compiler
warning**. The workspace is kept at zero warnings under `--all-targets`, which compiles the library,
the binaries and the test code, so a dead helper, an unused binding or a lost integration fails the
build instead of silently accumulating. Platform-specific code (Android) and feature-specific code
(`local-piper-tts`) is gated with `#[cfg]`/`#[cfg_attr]`, so the host check stays meaningful.

Reproduce locally:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --message-format=short
```

## Not verified by CI (release tasks)

- Real-model inference on device: Spleeter separation quality, Whisper keyword timing and Kokoro WAV
  playback must be exercised on Windows 10 and a physical Android device; CI only compiles and unit-tests.
- ONNX Runtime native libraries (`ort` loads dynamically) and DirectML availability are packaging-time
  concerns, not compile-time ones.
- `audio-embedding-int8.onnx` and all BYOM models: the app validates tensor shapes and local paths; it
  cannot and does not verify licensing, provenance or numeric parity of user-supplied weights.
- Commercial rights to Kokoro voice data, the optional Piper feature (GPL-3.0 gating) and any BYOM
  weights remain legal review items; see `docs/model-registry.md` and `CODE_GOVERNANCE.md`.

## Re-running locally

```sh
npm ci && npm run build && npm test          # frontend
cargo fmt --all -- --check && cargo test --workspace   # Rust (needs the Linux audio/Tauri deps)
scripts/verify-model-pins.sh --report reports/model-pin-verification.json   # strict pin verification
scripts/verify-model-pins.sh --allow-pending                                # discovery mode
python3 scripts/generate-cc0-reggae-drops.py --check                        # sampleboard byte-exactness
scripts/clean-untracked.sh                                                 # list untracked paths (dry run)
scripts/clean-untracked.sh --force                                         # remove them; tracked and ignored paths stay
```
