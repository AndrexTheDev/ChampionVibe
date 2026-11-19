# Release checklist

**Goal:** a usable first Windows 10 and Android release. A green automated check is not a device test.

## 1. Development checks — no installers

- [ ] On the current candidate, run `npm ci`, `npm test`, `npm run e2e:tauri`, `python3 tests/onnx_runtime_test.py`, `npm run build`, and `npm audit`. The headless IPC test uses a stub; it checks the bridge contract but cannot play native audio.
- [ ] Confirm the Linux Rust format, check and test jobs pass with the checked-in lockfile; confirm the offline model compliance gate passes.
- [ ] Verify the actual Android AAR and the redistribution notices for the runtime libraries before freezing the release candidate. Packaging now stages ONNX Runtime 1.24.3; see [desktop and Android runtime packaging](native-onnx-runtime.md) for the remaining verification.
- [ ] Decide whether Pro activation is in scope for this release. If so, check the issuer configuration and use a controlled end-to-end activation test before public distribution. Verify that Demo cannot start a recording.

## 2. Build the final candidate once

- [ ] Freeze the candidate revision. Run [Final packaging](github-actions-artifacts.md#start-final-packaging) manually; ordinary pushes and pull requests only run checks.
- [ ] Download both artifacts from that exact revision. Verify which artifacts are signed and suitable for installation and distribution. An Actions artifact is not a public release.

## 3. Validate the actual installers before publishing

- [ ] Clean install on Windows 10 and on a physical arm64 Android device (minimum supported Android API: 26). Record device/OS, commit and pass/fail for each test.
- [ ] Import a local track/folder on each platform; check Android's file picker and track playback after an app restart. Confirm the first-run model downloads and stem separation without a developer-supplied runtime. Exercise automatic transition, live beat-grid correction, and offline playback of already imported tracks.
- [ ] With a valid Pro licence (if included), record and play back a mix. Check device audio interruption/recovery and TTS/sample playback. Capture logs for failures without including private information.
- [ ] If a blocker is found, fix it and repeat the checks before producing a replacement candidate. Do not publish an untested package.
