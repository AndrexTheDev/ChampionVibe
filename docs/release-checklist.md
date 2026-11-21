# Release checklist

**Goal:** a usable first Windows 10 and Android release. A green automated check is not a device test.

## 1. Development checks — no installers

- [ ] On the current candidate, run `npm ci`, `npm test`, `npm run e2e:tauri`, `npm run e2e:android` (after building the app frontend), `python3 tests/onnx_runtime_test.py`, `python3 tests/android_release_signing_test.py`, `npm run build`, `npm run build:site`, and `npm audit`. The headless IPC test uses a stub; it checks the bridge contract but cannot play native audio.
- [ ] Confirm the Linux Rust format, check and test jobs pass with the checked-in lockfile; confirm the offline model compliance gate passes.
- [ ] Confirm the [Android compile check](BUILD_VERIFICATION.md#android-compile-check-no-apk) builds the ARM64 library, exports the JNI audio bridge, and compiles the generated Tauri app and its Android plugins. No APK is needed for this step.
- [ ] Run the [Native runtime preflight](native-onnx-runtime.md#check-the-real-provider-archives-without-making-an-apk) and verify the redistribution notices for the runtime libraries before freezing the release candidate. Packaging now stages ONNX Runtime 1.24.3; see [desktop and Android runtime packaging](native-onnx-runtime.md) for the remaining verification.
- [ ] Decide whether Pro activation is in scope for this release. If so, check the issuer configuration and use a controlled end-to-end activation test before public distribution. Verify that Demo cannot start a recording.

## 2. Build the final candidate once

- [ ] Set up the protected [Android signing environment](android-release-signing.md) and decide on a Windows Authenticode signing identity before public distribution.
- [ ] Freeze the candidate revision. Run [Final packaging](github-actions-artifacts.md#start-final-packaging) manually; ordinary pushes and pull requests only run checks.
- [ ] Download both artifacts from that exact revision. Check the published SHA-256 checksums and verify which artifacts are signed and suitable for distribution. An Actions artifact is not a public release.

## 3. Validate the actual installers before publishing (deferred during development)

- [ ] Clean install on Windows 10 and on a physical arm64 Android device (minimum supported Android API: 26). Record device/OS, commit and pass/fail for each test.
- [ ] Import a local track/folder on each platform; check Android's file picker and track playback after an app restart. Confirm the first-run model downloads and stem separation without a developer-supplied runtime. Exercise automatic transition, live beat-grid correction, and offline playback of already imported tracks.
- [ ] With a valid Pro licence (if included), record and play back a mix. Check device audio interruption/recovery and TTS/sample playback. Capture logs for failures without including private information.
- [ ] If a blocker is found, fix it and repeat the checks before producing a replacement candidate. Do not publish an untested package.
