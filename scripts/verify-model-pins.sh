#!/usr/bin/env bash
# Verifies every SHA-256 pin in src-tauri/assets/model-registry.json against the bytes the
# release actually serves, and cross-checks the upstream checksum file when one is listed.
#
# Modes:
#   scripts/verify-model-pins.sh                 # strict: a wrong pin fails the run
#   scripts/verify-model-pins.sh --allow-pending # print the real hash for unverified entries
#                                                # and only fail on a *wrong* pin
#
# Run this in CI after changing the registry, and locally before a release.
set -euo pipefail

registry="src-tauri/assets/model-registry.json"
allow_pending=0
for argument in "$@"; do
  case "$argument" in
    --allow-pending) allow_pending=1 ;;
    -h | --help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $argument" >&2
      exit 2
      ;;
  esac
done

if [[ ! -f "$registry" ]]; then
  echo "Error: $registry is missing." >&2
  exit 1
fi

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Error: '$1' is required." >&2
    exit 1
  }
}

require_command curl
require_command sha256sum
require_command python3

failures=0
pending=0
verified=0

# Read id|url|sha256|size|checksumSource tuples without depending on jq.
mapfile -t entries < <(
  python3 - "$registry" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    registry = json.load(handle)

for artifact in registry["artifacts"]:
    fields = [
        artifact["id"],
        artifact["url"],
        artifact["sha256"],
        str(artifact["sizeBytes"]),
        artifact.get("checksumSource", ""),
    ]
    print("\t".join(fields))
PY
)

if [[ ${#entries[@]} -eq 0 ]]; then
  echo "Error: the registry lists no artifacts." >&2
  exit 1
fi

# A cleared registry must not point at uncleared separator sources.
mapfile -t forbidden < <(
  python3 - "$registry" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    registry = json.load(handle)

print("\n".join(registry["policy"]["forbiddenDownloadSources"]))
PY
)

for entry in "${entries[@]}"; do
  IFS=$'\t' read -r id url sha256 size checksum_source <<<"$entry"

  for source in "${forbidden[@]}"; do
    if [[ -n "$source" && "$url" == *"$source"* ]]; then
      echo "FAIL  $id uses the forbidden download source '$source'." >&2
      failures=$((failures + 1))
      continue
    fi
  done

  echo "== $id"
  echo "   url    $url"
  download="$(mktemp)"
  trap 'rm -f "$download"' EXIT

  if ! curl --fail --location --silent --show-error --retry 3 --max-time 900 \
    --output "$download" "$url"; then
    echo "FAIL  $id could not be downloaded for verification." >&2
    failures=$((failures + 1))
    rm -f "$download"
    continue
  fi

  actual_size="$(wc -c <"$download" | tr -d ' ')"
  actual_hash="$(sha256sum "$download" | cut -d' ' -f1)"
  echo "   size   $actual_size (registry: $size)"
  echo "   sha256 $actual_hash"

  if [[ "$actual_size" != "$size" ]]; then
    echo "FAIL  $id byte size drifted from the registry." >&2
    failures=$((failures + 1))
  fi

  if [[ "$sha256" == "PENDING_VERIFICATION" ]]; then
    if [[ "$allow_pending" == "1" ]]; then
      echo "PENDING $id is not pinned yet. Set sha256 to $actual_hash in $registry."
      pending=$((pending + 1))
    else
      echo "FAIL  $id has no verified pin. Set sha256 to $actual_hash in $registry." >&2
      failures=$((failures + 1))
    fi
  elif [[ "$actual_hash" != "$sha256" ]]; then
    echo "FAIL  $id does not match its SHA-256 pin." >&2
    failures=$((failures + 1))
  else
    echo "OK    $id matches its SHA-256 pin."
    verified=$((verified + 1))
  fi

  if [[ -n "$checksum_source" ]]; then
    upstream="$(mktemp)"
    if curl --fail --location --silent --show-error --retry 3 --max-time 300 \
      --output "$upstream" "$checksum_source"; then
      if grep -Fqi "$actual_hash" "$upstream"; then
        echo "OK    upstream checksum file confirms this hash."
      else
        echo "WARN  upstream checksum file does not list this hash; inspect $checksum_source manually."
      fi
    else
      echo "WARN  upstream checksum file could not be fetched."
    fi
    rm -f "$upstream"
  fi

  rm -f "$download"
done

echo
echo "verified=$verified pending=$pending failures=$failures"
[[ "$failures" -eq 0 ]] || exit 1
