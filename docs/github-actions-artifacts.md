# Downloading Windows and Android builds

`.github/workflows/ci.yml` runs on every `push` (all branches), every `pull_request`, and manual `workflow_dispatch`. The `package` matrix uploads two Actions artifacts:

- `championvibe-windows-test-build` — Windows `.msi` and NSIS `.exe` installers.
- `championvibe-android-apk` — Android `.apk` for `arm64-v8a` (64-bit ARM). The CI excludes 32-bit ABIs because the current `whisper-rs-sys` bundled bindings do not compile for them; the APK therefore requires a 64-bit ARM Android device.

Actions artifacts are retained for 14 days. **The current workflow does not publish GitHub Release assets**; it publishes run artifacts only. The download script supports both, so it can also fetch assets after a Release is created and populated separately.

## First Actions run

Push a commit to the branch you want to validate. Once the workflow completes, find its run ID with:

```sh
gh run list --workflow CI --branch <branch-name> --limit 5
```

Watch a run and fail the command if a job fails:

```sh
gh run watch <run-id> --exit-status
```

The CI includes frontend tests/build, Rust formatting/checks/tests, and Windows/Android packaging. Package jobs need the corresponding GitHub-hosted runner prerequisites and may take up to two hours.

## Download the artifacts

Install and authenticate GitHub CLI (`gh auth login`), then download both package artifacts from a successful run:

```sh
scripts/download-actions-builds.sh <run-id> [destination]
```

For example:

```sh
scripts/download-actions-builds.sh 123456789 ./downloads/championvibe
```

To choose the newest successful run for the current branch automatically:

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
