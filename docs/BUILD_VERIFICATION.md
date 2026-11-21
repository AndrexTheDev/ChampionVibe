# Build verification

What CI proves about a ChampionVibe commit, and what it cannot prove yet.

## Jobs

| Workflow / job | Runs | Proves |
| --- | --- | --- |
| `ci.yml` · `frontend` | `npm ci`, `npm test`, `npm run e2e:tauri`, `python3 tests/onnx_runtime_test.py`, `npm run build` | The WebView build compiles and all frontend suites pass, including the model-library, i18n and BYOM contract tests; the IPC driver checks the command and event contract against a stub, not a native runtime |
| `ci.yml` · `compliance` | `scripts/verify-cleared-model-policy.sh` | Offline policy gate: registry tiers, SHA-256 pins and forbidden sources, no bundled weights, BYOM acknowledgement rules, byte-exact CC0 sampleboard, locale transparency wording, and the locked/warning-free build wiring |
| `ci.yml` · `rust` | `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --locked`, `cargo test --workspace --locked` | Formatting, warning-free compilation of library, binaries and tests on Linux with Tauri/audio system dependencies, all Rust unit tests (`model_loader`, `samples`, Spleeter frame/STFT bookkeeping), and that the checked-in `Cargo.lock` is the graph the build actually uses |
| `android-native.yml` · `arm64` | Generate the Tauri project, build the locked ARM64 Rust library with NDK r28c/API 26, and run Gradle's release Kotlin and Java compile tasks | Checks the generated Android project and plugin sources, JNI audio export, 16 KiB ELF alignment, and staging of the NDK C++ runtime. No APK. |
| `package.yml` (manual only) · `package` | Tauri bundling on `windows-latest` and Android `aarch64-linux-android` | Windows MSI/NSIS and Android APK artifacts (`championvibe-windows-test-build`, `championvibe-android-apk`, 14-day retention) build after CI passes for the selected commit |
| `model-pins.yml` · `lockfile` | `cargo generate-lockfile`, `::notice` annotation, commit-back | `Cargo.lock` tracks the full workspace dependency graph and is committed to the branch; the job reports the lock SHA-256 and package count |
| `model-pins.yml` · `model-pins` | `scripts/verify-model-pins.sh` + forbidden-source check | Every downloadable artifact matches its registry byte size and SHA-256, no forbidden separator source is reachable, and `reports/model-pin-verification.json` is refreshed |

## What the pin job actually establishes

The strict verifier downloads each artifact from its pinned URL and fails on size drift, hash drift, a
reachable forbidden source, or a pin that is still `PENDING_VERIFICATION`. When the upstream release
publishes a checksum file that lists the artifact, that value is cross-checked too; for the
`sherpa-onnx-spleeter-2stems*` archives k2-fsa's `checksum.txt` does not list them, so those pins rest
on the byte-exact CI download plus the recorded size, timestamp and run reference.

## Cleared-model compliance gate

`scripts/verify-cleared-model-policy.sh` (also `npm run compliance:check`) is the offline counterpart
to the network-dependent pin verification. It fails on:

* a registry artifact that is not in the cleared tier, has no 64-hex SHA-256 pin, no byte size or no
  rights/notice documentation;
* an artifact URL that resolves to a forbidden download source (`facebookresearch/demucs`,
  `sigsep/open-unmix-pytorch`, the KUIELab/UVR mirrors, `TRvlvr/model_repo`);
* a default separator that is missing from the registry or not cleared;
* a BYOM kind that needs an acknowledgement but carries no rights warning;
* tracked `*.onnx`, `*.tar.bz2`, `*.tar.gz`, `*.ckpt` or `*.pth` files (bundled weights);
* a sampleboard that is not the ten committed CC0 WAVs, byte for byte;
* a locale that lacks the transparent TTS label or the "not authentic Patois" disclosure, or whose
  key set drifts from `de.json`;
* a `Cargo.lock` that is not checked in, or CI commands that stopped using `--locked`, stopped
  failing on warnings, or stopped loading the embedded registry through the guarded path.

It runs in the `compliance` job and in the frontend suite (`src/compliance.test.js`), so a
policy regression fails the build even before the pin job downloads anything. Reproduce locally:

```sh
scripts/verify-cleared-model-policy.sh
```

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
cargo check --workspace --all-targets --locked --message-format=short
cargo test --workspace --locked
```

## Android UI regression check (no APK)

```sh
npm ci
npm run build
npx --no-install playwright-core install --only-shell chromium
npm run e2e:android
```

The check serves the actual built app in Chromium at Android phone sizes with an Android user agent and stubbed Tauri IPC. It exercises immediate access to Demo, the recording gate, the audio file/folder picker contract (including cancellation), the native single-file ONNX picker and the import request. It checks portrait overflow at 320, 360 and 412 CSS pixels in all four languages. A compatible preinstalled browser can be selected with `CHROMIUM_EXECUTABLE`. This is not the Android system WebView or a real device; it does not validate Android permissions, model inference, or audio output.

## Android compile check (no APK)

The **Android compile check** runs for relevant pull requests, on manual dispatch, and before final packaging. It generates the Tauri Android project with the locked npm CLI. The shared `scripts/prepare-android-gradle.sh` prepares both the compile check and manual packaging with Android SDK 36: the installed Tauri CLI template declares SDK 37, which the runner's stable SDK repository did not offer. This does not change the Android 8.0 (API 26) minimum.

Cargo cross-compiles the ARM64 app library and runs the Tauri/Kotlin code generators. CI checks the ELF load segments, the JNI audio bridge export and staging of the real NDK C++ runtime with its notices. Gradle then runs `:app:compileArm64ReleaseKotlin` and `:app:compileArm64ReleaseJavaWithJavac`, including the Android libraries referenced by the generated project. Gradle does not run APK assembly or signing. Device permissions, audio and the final package remain outside this check.

For the exact commands and the Tauri code-generation environment, see [the workflow](../.github/workflows/android-native.yml). To rerun it without building an installer, use **Android compile check → Run workflow** in GitHub Actions.

## Not verified by CI (release tasks)

- Real-model inference on device: Spleeter separation quality, Whisper keyword timing and Kokoro WAV
  playback must be exercised on Windows 10 and a physical Android device; CI only compiles and unit-tests.
- The native ONNX Runtime is staged during final packaging; ordinary CI checks the staging logic with offline fixtures. Whether the packaged libraries load and DirectML works on a given Windows 10 device still requires a device test. See [runtime packaging](native-onnx-runtime.md).
- `audio-embedding-int8.onnx` and all BYOM models: the app validates tensor shapes and local paths; it
  cannot and does not verify licensing, provenance or numeric parity of user-supplied weights.
- Commercial rights to Kokoro voice data, the optional Piper feature (GPL-3.0 gating) and any BYOM
  weights remain legal review items; see `docs/model-registry.md` and `CODE_GOVERNANCE.md`.

## Re-running locally

```sh
npm ci && npm run build && npm test          # frontend
cargo fmt --all -- --check && cargo test --workspace --locked  # Rust (needs the Linux audio/Tauri deps)
scripts/verify-model-pins.sh --report reports/model-pin-verification.json   # strict pin verification
scripts/verify-model-pins.sh --allow-pending                                # discovery mode
python3 scripts/generate-cc0-reggae-drops.py --check                        # sampleboard byte-exactness
scripts/verify-cleared-model-policy.sh                                      # offline policy gate
scripts/clean-untracked.sh                                                 # list untracked paths (dry run)
scripts/clean-untracked.sh --force                                         # remove them; tracked and ignored paths stay
```
