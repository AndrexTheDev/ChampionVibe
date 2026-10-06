#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/download-actions-builds.sh latest [destination]
  scripts/download-actions-builds.sh <run-id> [destination]
  scripts/download-actions-builds.sh release [tag] [destination]

Downloads the Windows and Android CI artifacts from a successful Actions run, or all assets
from a GitHub Release. Requires GitHub CLI (`gh`) authentication and the Actions read permission.
Set GH_REPO=OWNER/REPOSITORY to override repository detection.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "Error: GitHub CLI 'gh' is required." >&2
  exit 1
fi

repo="${GH_REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
mode="${1:-latest}"

if [[ "$mode" == "release" ]]; then
  tag="${2:-}"
  destination="${3:-artifacts/github-release-${tag:-latest}}"
  mkdir -p "$destination"
  if [[ -n "$tag" && "$tag" != "latest" ]]; then
    gh release download "$tag" --repo "$repo" --dir "$destination"
  else
    gh release download --repo "$repo" --dir "$destination" --pattern '*'
  fi
  echo "Release assets downloaded to: $destination"
  find "$destination" -type f -print
  exit 0
fi

if [[ "$mode" == "latest" ]]; then
  branch="${GH_RUN_BRANCH:-$(git branch --show-current)}"
  if [[ -z "$branch" ]]; then
    echo "Error: cannot determine the current Git branch; set GH_RUN_BRANCH." >&2
    exit 1
  fi
  run_id="$(gh run list \
    --workflow CI \
    --repo "$repo" \
    --branch "$branch" \
    --limit 30 \
    --json databaseId,status,conclusion \
    --jq '[.[] | select(.status == "completed" and .conclusion == "success")][0].databaseId // empty')"
  if [[ -z "$run_id" ]]; then
    echo "Error: no successful CI run was found for branch '$branch'. Pass a run ID after the workflow completes." >&2
    exit 1
  fi
else
  if [[ ! "$mode" =~ ^[0-9]+$ ]]; then
    usage >&2
    exit 2
  fi
  run_id="$mode"
fi

destination="${2:-artifacts/github-actions-${run_id}}"
mkdir -p "$destination/windows" "$destination/android"

gh run download "$run_id" \
  --repo "$repo" \
  --name championvibe-windows-test-build \
  --dir "$destination/windows"
gh run download "$run_id" \
  --repo "$repo" \
  --name championvibe-android-apk \
  --dir "$destination/android"

downloaded_installer="$(find "$destination" -type f \( -iname '*.msi' -o -iname '*.exe' -o -iname '*.apk' \) -print -quit)"
if [[ -z "$downloaded_installer" ]]; then
  echo "Error: downloaded artifacts contain no MSI, EXE, or APK files." >&2
  exit 1
fi

echo "Build artifacts from Actions run $run_id downloaded to: $destination"
find "$destination" -type f \( -iname '*.msi' -o -iname '*.exe' -o -iname '*.apk' \) -print
