# Downloading Windows and Android builds

Normal pushes and pull requests run `ci.yml` without building any installers. The separate **Final packaging** workflow (`package.yml`) runs only when started manually. It first runs CI for that revision, then builds and uploads artifacts for the platform selected at dispatch:

- `championvibe-windows-test-build` — Windows `.msi` and NSIS `.exe` installers.
- `championvibe-android-apk` — a signed Android `.apk` for `arm64-v8a` (64-bit ARM). The CI excludes 32-bit ABIs because the current `whisper-rs-sys` bundled bindings do not compile for them; the APK therefore requires a 64-bit ARM Android device.

Each artifact includes a `SHA256SUMS.txt` file for checking the downloaded installers (`sha256sum -c SHA256SUMS.txt` from the extracted platform directory, or `shasum -a 256 -c SHA256SUMS.txt` on macOS). Actions artifacts are retained for 14 days. **The current workflow does not publish GitHub Release assets**; it publishes run artifacts only. The download script supports both, so it can also fetch assets after a Release is created and populated separately.

## Start final packaging

Wait until the candidate is ready according to the [release checklist](release-checklist.md). Configure the protected [Android release signing environment](android-release-signing.md) first; without a valid configuration the Android job stops without building an APK. For manual dispatch, GitHub requires the workflow file to be present on the default branch. Select the candidate ref in **Actions → Final packaging → Run workflow**, then choose `android` (the default), `windows`, or `both`. An Android-only run does not build Windows installers. A Windows-only run skips the Android compile gate and does not require Android credentials. To run the Android job only:

```sh
gh workflow run package.yml --ref <candidate-branch-or-tag> -f platform=android
gh run list --workflow package.yml --branch <candidate-branch> --limit 5
gh run watch <run-id> --exit-status
```

The package jobs use Windows and Android build tools and can each take up to two hours. Do not dispatch this workflow just to check an ordinary change.

## Download the artifacts

Install and authenticate GitHub CLI (`gh auth login`), then download the available package artifacts from a successful run (Android, Windows, or both):

```sh
scripts/download-actions-builds.sh <run-id> [destination]
```

For example:

```sh
scripts/download-actions-builds.sh 123456789 ./downloads/championvibe
```

To choose the newest successful **Final packaging** run for the current branch automatically:

```sh
scripts/download-actions-builds.sh latest
```

You can also download all assets attached to a GitHub Release:

```sh
scripts/download-actions-builds.sh release <tag> [destination]
# or the latest Release:
scripts/download-actions-builds.sh release
```

The script uses the workflow's artifact names, places run downloads in separate `windows/` and `android/` folders, and verifies that an installer/APK was downloaded. Set `GH_REPO=OWNER/REPOSITORY` if GitHub CLI cannot infer the repository from the checkout. If you prefer the browser, open the Actions run and download each artifact from its **Artifacts** section before the 14-day retention period expires.
