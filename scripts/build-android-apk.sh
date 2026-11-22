#!/usr/bin/env bash
# Run from the repository root, on Linux, after installing the Android SDK and NDK.
set -euo pipefail
config=()
if [[ -n ${1:-} ]]; then
  config=(--config "$1")
fi
npm run tauri -- android init --ci --skip-targets-install "${config[@]}"
project=src-tauri/gen/android
bash scripts/prepare-android-gradle.sh "$project"
python3 scripts/stage-onnx-runtime.py android
python3 scripts/stage-onnx-runtime.py stage-android-cxx --ndk "$ANDROID_NDK_HOME" --dest "$project/app/src/main"
install -Dm644 -t "$project/app/src/main/assets/championvibe-licenses/listenablefuture-1.0" \
  legal/android/listenablefuture-1.0/{COPYING,ATTRIBUTION.txt}
npm run tauri -- android build --ci --apk --target aarch64 "${config[@]}"
