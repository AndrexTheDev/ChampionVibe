#!/usr/bin/env bash
# Use the generated app and local bridge declarations; fail on template drift.
set -euo pipefail
app=${1:?Provide the generated Android project directory}
platforms=()
for file in "$app/app/build.gradle.kts" crates/device-identity/android/build.gradle.kts; do
  platform=$(sed -nE 's/^[[:space:]]*compileSdk[[:space:]]*=[[:space:]]*([0-9]+)[[:space:]]*$/\1/p' "$file")
  if [[ ! "$platform" =~ ^[0-9]+$ ]]; then
    echo "Expected one numeric compileSdk in $file" >&2
    exit 1
  fi
  if [[ " ${platforms[*]} " != *" platforms;android-$platform "* ]]; then
    platforms+=("platforms;android-$platform")
  fi
done
sdkmanager --install "${platforms[@]}"
