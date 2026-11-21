#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/download-actions-builds.sh latest [destination]
  scripts/download-actions-builds.sh <run-id> [destination]
  scripts/download-actions-builds.sh release [tag] [destination]

Downloads the available Windows and/or Android artifacts from a successful Actions run, or all assets
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
    --workflow package.yml \
    --repo "$repo" \
    --branch "$branch" \
    --limit 30 \
    --json databaseId,status,conclusion \
    --jq '[.[] | select(.status == "completed" and .conclusion == "success")][0].databaseId // empty')"
  if [[ -z "$run_id" ]]; then
    echo "Error: no successful final packaging run was found for branch '$branch'. Pass a run ID after the workflow completes." >&2
    exit 1
  fi
else
  if [[ ! "$mode" =~ ^[0-9]+$ ]]; then
    usage >&2
    exit 2
  fi
  run_id="$mode"
fi

if [[ "$(gh run view "$run_id" --repo "$repo" --json conclusion --jq .conclusion)" != "success" ]]; then
  echo "Error: Actions run $run_id did not finish successfully." >&2
  exit 1
fi

destination="${2:-artifacts/github-actions-${run_id}}"
artifact_names="$(gh api --paginate "repos/$repo/actions/runs/$run_id/artifacts" \
  --jq '.artifacts[] | select(.expired == false) | .name')"
downloaded=0
for platform in windows android android-test; do
  case "$platform" in
    windows) artifact=championvibe-windows-test-build; suffix=msi ;;
    android) artifact=championvibe-android-apk; suffix=apk ;;
    android-test) artifact=championvibe-android-test-apk; suffix=apk ;;
  esac
  if ! grep -Fxq "$artifact" <<< "$artifact_names"; then
    continue
  fi
  dir="$destination/$platform"
  mkdir -p "$dir"
  gh run download "$run_id" --repo "$repo" --name "$artifact" --dir "$dir"
  if [[ ! -s "$dir/SHA256SUMS.txt" || -z "$(find "$dir" -maxdepth 1 -type f -iname "*.$suffix" -print -quit)" ]]; then
    echo "Error: $artifact contains no .$suffix installer or checksum file." >&2
    exit 1
  fi
  if [[ "$platform" == windows && -z "$(find "$dir" -maxdepth 1 -type f -iname '*.exe' -print -quit)" ]]; then
    echo "Error: $artifact contains no NSIS installer." >&2
    exit 1
  fi
  downloaded=$((downloaded + 1))
  echo "Downloaded $artifact to $dir"
done
if [[ "$downloaded" -eq 0 ]]; then
  echo "Error: no unexpired ChampionVibe package artifacts were found for Actions run $run_id." >&2
  exit 1
fi
