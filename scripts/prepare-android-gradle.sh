#!/usr/bin/env bash
# Use the same stable SDK for the compile check and the release package.
set -euo pipefail
project=${1:?Provide the generated Android project directory}
platform=36
app_gradle="$project/app/build.gradle.kts"
bridge_gradle=crates/device-identity/android/build.gradle.kts
for key in compileSdk targetSdk; do
  declared=$(sed -nE "s/^[[:space:]]*$key[[:space:]]*=[[:space:]]*([0-9]+)[[:space:]]*$/\1/p" "$app_gradle")
  if [[ ! "$declared" =~ ^[0-9]+$ ]]; then
    echo "Expected one numeric $key in $app_gradle" >&2
    exit 1
  fi
done
bridge_sdk=$(sed -nE 's/^[[:space:]]*compileSdk[[:space:]]*=[[:space:]]*([0-9]+)[[:space:]]*$/\1/p' "$bridge_gradle")
if [[ "$bridge_sdk" != "$platform" ]]; then
  echo "Expected compileSdk $platform in $bridge_gradle; update the Android SDK pin deliberately" >&2
  exit 1
fi
# Tauri CLI 2.12.1 emits SDK 37, which the runner's stable SDK repository does not offer.
sdkmanager --install "platforms;android-$platform"
sed -Ei "s/^([[:space:]]*(compileSdk|targetSdk)[[:space:]]*=[[:space:]]*)[0-9]+[[:space:]]*$/\1$platform/" "$app_gradle"

# The Tauri buildSrc Kotlin DSL plugin uses the Plugin Portal, but its Kotlin
# dependencies are published on Maven Central. Avoid relying on Portal redirects.
if [[ ! -f "$project/buildSrc/settings.gradle" && ! -f "$project/buildSrc/settings.gradle.kts" ]]; then
  cat > "$project/buildSrc/settings.gradle.kts" <<'GRADLE'
pluginManagement {
    repositories {
        mavenCentral()
        gradlePluginPortal()
    }
}
GRADLE
fi
