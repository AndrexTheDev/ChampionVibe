#!/usr/bin/env bash
# Verifies every SHA-256 pin in src-tauri/assets/model-registry.json against the bytes the
# release actually serves, and cross-checks the upstream checksum file when one is listed.
#
# Modes:
#   scripts/verify-model-pins.sh                  # strict: a wrong or missing pin fails the run
#   scripts/verify-model-pins.sh --allow-pending  # report unverified entries, fail only on a wrong pin
#   scripts/verify-model-pins.sh --report out.json
#                                                 # additionally write the discovered hashes to JSON
#
# Run this in CI after changing the registry, and locally before a release.
set -euo pipefail

registry="src-tauri/assets/model-registry.json"
allow_pending=0
report_path=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --allow-pending)
      allow_pending=1
      shift
      ;;
    --report)
      report_path="${2:-}"
      if [[ -z "$report_path" ]]; then
        echo "Error: --report needs a path." >&2
        exit 2
      fi
      shift 2
      ;;
    -h | --help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [[ ! -f "$registry" ]]; then
  echo "Error: $registry is missing." >&2
  exit 1
fi

for command in curl sha256sum python3; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "Error: '$command' is required." >&2
    exit 1
  }
done

work_dir="$(mktemp -d)"
results="$work_dir/results.tsv"
: >"$results"
cleanup() { rm -rf "$work_dir"; }
trap cleanup EXIT

failures=0
pending=0
verified=0

mapfile -t entries < <(
  python3 - "$registry" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    registry = json.load(handle)

for artifact in registry["artifacts"]:
    print("\t".join([
        artifact["id"],
        artifact["url"],
        artifact["sha256"],
        str(artifact["sizeBytes"]),
        artifact.get("checksumSource", ""),
    ]))
PY
)

mapfile -t forbidden < <(
  python3 - "$registry" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    registry = json.load(handle)

print("\n".join(registry["policy"]["forbiddenDownloadSources"]))
PY
)

if [[ ${#entries[@]} -eq 0 ]]; then
  echo "Error: the registry lists no artifacts." >&2
  exit 1
fi

for entry in "${entries[@]}"; do
  IFS=$'\t' read -r id url sha256 size checksum_source <<<"$entry"
  echo "== $id"
  echo "   url    $url"

  forbidden_hit=""
  for source in "${forbidden[@]}"; do
    if [[ -n "$source" && "$url" == *"$source"* ]]; then
      forbidden_hit="$source"
    fi
  done
  if [[ -n "$forbidden_hit" ]]; then
    echo "FAIL  $id downloads from the forbidden source '$forbidden_hit'." >&2
    failures=$((failures + 1))
    printf '%s\t%s\t%s\t\t%s\tforbidden-source\n' "$id" "$url" "$sha256" "$forbidden_hit" >>"$results"
    continue
  fi

  download="$work_dir/$id.bin"
  if ! curl --fail --location --silent --show-error --retry 3 --max-time 900 \
    --output "$download" "$url"; then
    echo "FAIL  $id could not be downloaded for verification." >&2
    failures=$((failures + 1))
    printf '%s\t%s\t%s\t\t\tunreachable\n' "$id" "$url" "$sha256" >>"$results"
    continue
  fi

  actual_size="$(wc -c <"$download" | tr -d ' ')"
  actual_hash="$(sha256sum "$download" | cut -d' ' -f1)"
  size_status="ok"
  if [[ "$actual_size" != "$size" ]]; then
    size_status="size-mismatch"
    echo "FAIL  $id byte size drifted: $actual_size != $size." >&2
    failures=$((failures + 1))
  fi
  echo "   size   $actual_size (registry: $size)"
  echo "   sha256 $actual_hash"

  status="verified"
  if [[ "$sha256" == "PENDING_VERIFICATION" ]]; then
    status="pending"
    if [[ "$allow_pending" == "1" ]]; then
      echo "PENDING $id has no verified pin yet. Registry value should become: $actual_hash"
      pending=$((pending + 1))
    else
      echo "FAIL  $id has no verified pin. Registry value should become: $actual_hash" >&2
      failures=$((failures + 1))
    fi
  elif [[ "$actual_hash" != "$sha256" ]]; then
    status="mismatch"
    echo "FAIL  $id does not match its SHA-256 pin." >&2
    failures=$((failures + 1))
  else
    echo "OK    $id matches its SHA-256 pin."
    verified=$((verified + 1))
  fi

  upstream_status="not-checked"
  if [[ -n "$checksum_source" ]]; then
    upstream="$work_dir/$id.checksum"
    if curl --fail --location --silent --show-error --retry 3 --max-time 300 \
      --output "$upstream" "$checksum_source"; then
      if grep -Fqi "$actual_hash" "$upstream"; then
        upstream_status="confirmed"
        echo "OK    upstream checksum file confirms this hash."
      else
        upstream_status="not-listed"
        echo "WARN  upstream checksum file does not list this hash; inspect $checksum_source manually."
      fi
    else
      upstream_status="unreachable"
      echo "WARN  upstream checksum file could not be fetched."
    fi
  fi

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$id" "$url" "$sha256" "$actual_hash" "$actual_size" "$size_status" "$status" "$upstream_status" >>"$results"
  rm -f "$download"
done

if [[ -n "$report_path" ]]; then
  mkdir -p "$(dirname "$report_path")"
  python3 - "$registry" "$results" "$report_path" <<'PY'
import json
import sys
from datetime import datetime, timezone

registry_path, results_path, report_path = sys.argv[1], sys.argv[2], sys.argv[3]
with open(registry_path, "r", encoding="utf-8") as handle:
    registry = json.load(handle)

artifacts = []
with open(results_path, "r", encoding="utf-8") as handle:
    for line in handle:
        fields = line.rstrip("\n").split("\t")
        if len(fields) < 6:
            continue
        entry = {
            "id": fields[0],
            "url": fields[1],
            "registrySha256": fields[2],
        }
        if len(fields) >= 8:
            entry.update({
                "actualSha256": fields[3] or None,
                "actualSizeBytes": int(fields[4]) if fields[4].isdigit() else None,
                "sizeStatus": fields[5] or None,
                "status": fields[6] or None,
                "upstreamChecksum": fields[7] or None,
            })
        else:
            entry.update({
                "actualSha256": None,
                "actualSizeBytes": None,
                "sizeStatus": None,
                "status": fields[5] or None,
                "upstreamChecksum": None,
            })
        artifacts.append(entry)

report = {
    "format": "championvibe-model-pin-verification-v1",
    "generatedAt": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "registry": registry["format"],
    "artifacts": artifacts,
}
with open(report_path, "w", encoding="utf-8") as handle:
    json.dump(report, handle, indent=2)
    handle.write("\n")
print(f"wrote {report_path}")
PY
fi

echo
echo "verified=$verified pending=$pending failures=$failures"
[[ "$failures" -eq 0 ]] || exit 1
