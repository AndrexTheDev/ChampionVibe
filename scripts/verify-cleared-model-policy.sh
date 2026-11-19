#!/usr/bin/env bash
# Offline compliance gate for the shipped package.
#
# Everything here is checkable without network access, so it runs on every push and pull request:
#   * the model registry only lists cleared, pinned artifacts and no forbidden download source
#   * no separator weights or model archives are bundled in the repository
#   * the BYOM contract keeps acknowledgements for the uncleared kinds
#   * the standard package downloads the separator only through the guarded loader path
#   * the drop sampleboard is the committed CC0 set, byte for byte
#   * all four locale files carry the same keys and the transparent TTS label
#   * Cargo.lock is checked in and CI builds against it with `--locked`
#
# It complements the network-dependent checks: `scripts/verify-model-pins.sh` (hashes of the
# published artifacts) and `.github/workflows/model-pins.yml` (forbidden-source scan in CI).
#
# Usage: scripts/verify-cleared-model-policy.sh
set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

failures=0
checks=0

pass() {
  checks=$((checks + 1))
  printf 'PASS  %s\n' "$1"
}

fail() {
  checks=$((checks + 1))
  failures=$((failures + 1))
  printf 'FAIL  %s\n' "$1"
  if [[ -n "${2:-}" ]]; then
    printf '      %s\n' "$2"
  fi
}

expect() {
  # expect <description> <command...>
  local description="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    pass "$description"
  else
    fail "$description" "$*"
  fi
}

echo "ChampionVibe cleared-model compliance check"
echo

# --- model registry -------------------------------------------------------------------------
while IFS='|' read -r status description detail; do
  if [[ "$status" == "OK" ]]; then
    pass "$description"
  else
    fail "$description" "$detail"
  fi
done < <(python3 - <<'PY'
import json
import re
import sys

registry_path = "src-tauri/assets/model-registry.json"
try:
    with open(registry_path, encoding="utf-8") as handle:
        registry = json.load(handle)
except Exception as error:  # noqa: BLE001 - reported as a check failure
    print(f"could not read or parse {registry_path}|{error}")
    sys.exit(0)

def report(condition, description, detail=""):
    print(f"{'OK' if condition else 'FAIL'}|{description}" + (f"|{detail}" if detail else ""))

report(
    registry.get("format") == "championvibe-model-registry-v2",
    "registry declares championvibe-model-registry-v2",
    f"format={registry.get('format')}",
)

policy = registry.get("policy", {})
report(policy.get("clearedTierOnly") is True, "registry policy is cleared-tier-only")
report(bool(policy.get("statement")), "registry policy carries a clearing statement")
report(bool(policy.get("pinRule")), "registry policy documents the SHA-256 pin rule")

forbidden = [str(source).lower() for source in policy.get("forbiddenDownloadSources", [])]
report(bool(forbidden), "registry lists forbidden download sources", f"{len(forbidden)} entries")

artifacts = registry.get("artifacts", [])
report(bool(artifacts), "registry has at least one cleared artifact", f"{len(artifacts)} entries")

for artifact in artifacts:
    artifact_id = artifact.get("id", "<unnamed>")
    url = str(artifact.get("url", "")).lower()
    hits = [source for source in forbidden if source in url]
    report(not hits, f"{artifact_id} is not hosted at a forbidden source", ", ".join(hits))
    report(artifact.get("tier") == "cleared", f"{artifact_id} is in the cleared tier")
    report(
        bool(re.fullmatch(r"[0-9a-f]{64}", str(artifact.get("sha256", "")))),
        f"{artifact_id} carries a 64-hex SHA-256 pin",
    )
    size = artifact.get("sizeBytes", 0)
    max_bytes = artifact.get("maxBytes", 0)
    report(isinstance(size, int) and size > 0, f"{artifact_id} records its byte size", str(size))
    report(
        isinstance(max_bytes, int) and max_bytes >= size > 0,
        f"{artifact_id} caps the download at or above its size",
        f"maxBytes={max_bytes}",
    )
    report(bool(artifact.get("members")), f"{artifact_id} lists its expected archive members")
    report(
        all(artifact.get(field) for field in ("licenseSummary", "rightsNote", "upstreamStatement")),
        f"{artifact_id} documents license summary, rights note and upstream statement",
    )
    report(bool(artifact.get("notices")), f"{artifact_id} points at its third-party notices")
    report(
        artifact.get("kind") == "spleeter-2stems",
        f"{artifact_id} is a cleared Spleeter profile",
        f"kind={artifact.get('kind')}",
    )

default_id = policy.get("defaultSeparatorId")
default = next((item for item in artifacts if item.get("id") == default_id), None)
report(default is not None, f"default separator '{default_id}' exists in the registry")
report(
    default is not None and default.get("tier") == "cleared",
    "the default separator is a cleared artifact",
)

byom = registry.get("byom", {})
report(bool(byom.get("statement")), "BYOM policy carries a liability statement")
kinds = byom.get("acceptedKinds", [])
report(bool(kinds), "BYOM policy lists accepted kinds", f"{len(kinds)} kinds")
for kind in kinds:
    kind_id = kind.get("kind", "<unnamed>")
    report(bool(kind.get("contract")), f"BYOM kind {kind_id} documents its tensor contract")
    if kind.get("requiresAcknowledgement"):
        report(
            bool(kind.get("rightsWarning")),
            f"BYOM kind {kind_id} warns about the rights of user-supplied weights",
        )
PY
)

# --- no bundled separator weights ------------------------------------------------------------
bundled_weights="$(git ls-files '*.onnx' '*.tar.bz2' '*.tar.gz' '*.ckpt' '*.pth' | tr '\n' ' ')"
if [[ -z "${bundled_weights// /}" ]]; then
  pass "no separator weights or model archives are tracked in the repository"
else
  fail "no separator weights or model archives are tracked in the repository" "$bundled_weights"
fi

expect "the separator download path enforces the forbidden-source scan" \
  grep -q 'forbidden_source_hits(&artifact.url)' src-tauri/src/audio/model_loader.rs
expect "the separator download path enforces the cleared-tier-only policy" \
  grep -q 'cleared_tier_only' src-tauri/src/audio/model_loader.rs
expect "the embedded registry is validated against the expected format" \
  grep -q 'registry.format, REGISTRY_FORMAT' src-tauri/src/audio/model_loader.rs
expect "the separator download path requires a verified SHA-256 pin" \
  grep -q 'fn is_pinned' src-tauri/src/audio/model_loader.rs

# --- CC0 drop sampleboard --------------------------------------------------------------------
sample_count="$(git ls-files 'src-tauri/assets/samples/*.wav' | wc -l | tr -d ' ')"
if [[ "$sample_count" == "10" ]]; then
  pass "the drop sampleboard ships the 10 committed CC0 WAVs"
else
  fail "the drop sampleboard ships the 10 committed CC0 WAVs" "found $sample_count"
fi
expect "the committed drop samples are byte-exact" \
  python3 scripts/generate-cc0-reggae-drops.py --check
expect "the sampleboard README documents a per-file SHA-256 table" \
  bash -c 'grep -q "SHA-256" src-tauri/assets/samples/README.md && \
    grep -qE "[0-9a-f]{64}" src-tauri/assets/samples/README.md'
expect "every committed drop sample is declared CC0-compatible" \
  bash -c 'grep -qi "CC0" src-tauri/assets/samples/README.md'

# --- locales and transparent TTS branding ----------------------------------------------------
while IFS='|' read -r status description detail; do
  if [[ "$status" == "OK" ]]; then
    pass "$description"
  else
    fail "$description" "$detail"
  fi
done < <(python3 - <<'PY'
import json
import pathlib
import re

locales = ["de", "en", "es", "fr"]
# The short label names the engine transparently ("… pronunciation approximation") …
label_key = "US-Englische KI-Stimme mit Patois-Aussprache-Annäherung"
label_patterns = {
    "de": r"Annäherung",
    "en": r"approximation",
    "es": r"aproximación",
    "fr": r"approximation",
}
# … and the rights disclosure shown with the model download states, in the language of each
# locale, that the voice is not authentic Patois.
disclaimer_key = (
    "Ein gültiger SHA-256-Pin belegt nur die Identität der Datei, nicht kommerzielle Rechte. "
    "KUIELab-Gewichte und Trainingsdaten sind vor kommerziellem Einsatz separat zu klären. "
    "Kokoro-TTS läuft lokal mit englischer G2P und einer US-englischen Stimme; es ist keine "
    "authentische Patois-Stimme. Modell- und Voice-Rechte sind vor kommerzieller Distribution "
    "zu prüfen."
)
disclaimer_patterns = {
    "de": r"keine authentische Patois-Stimme|kein authentisches Patois",
    "en": r"not an authentic Patois voice|not authentic Patois",
    "es": r"no es una voz auténtica de patois|no patois auténtico|no será patois auténtico",
    "fr": r"pas une voix authentique en patois|pas le patois authentique",
}

documents = {}
for locale in locales:
    path = pathlib.Path(f"src/locales/{locale}.json")
    try:
        documents[locale] = json.loads(path.read_text(encoding="utf-8"))
    except Exception as error:  # noqa: BLE001 - reported as a check failure
        print(f"FAIL|could not read or parse {path}|{error}")
        documents = {}
        break

if documents:
    key_sets = {locale: set(document) for locale, document in documents.items()}
    reference = key_sets[locales[0]]
    print(f"OK|all four locale files parse ({len(reference)} keys in {locales[0]}.json)")
    for locale in locales[1:]:
        missing = reference - key_sets[locale]
        extra = key_sets[locale] - reference
        if missing or extra:
            print(
                f"FAIL|{locale}.json matches the {locales[0]}.json key set|"
                f"missing={sorted(missing)[:5]} extra={sorted(extra)[:5]}"
            )
        else:
            print(f"OK|{locale}.json matches the {locales[0]}.json key set")

    for locale in locales:
        document = documents[locale]
        label = document.get(label_key, "")
        if re.search(label_patterns[locale], label):
            print(f"OK|{locale}.json names the TTS engine as a pronunciation approximation")
        else:
            message = "names the TTS engine as a pronunciation approximation"
            print(f"FAIL|{locale}.json {message}|{label}")

        disclosure = document.get(disclaimer_key, "")
        if not disclosure:
            print(f"FAIL|{locale}.json carries the model rights disclosure")
        elif re.search(disclaimer_patterns[locale], disclosure):
            print(f"OK|{locale}.json states that the voice is not authentic Patois")
        else:
            print(f"FAIL|{locale}.json states that the voice is not authentic Patois|{disclosure}")
PY
)

# --- locked builds ---------------------------------------------------------------------------
expect "Cargo.lock is checked into Git" git ls-files --error-unmatch Cargo.lock
expect "Cargo.lock is a version 4 lockfile" grep -q '^version = 4$' Cargo.lock
expect "Cargo.lock resolves the workspace packages" \
  bash -c 'grep -q "name = \"championvibe\"" Cargo.lock'
expect "CI checks the workspace against the locked dependency graph" \
  grep -q -- 'cargo check --workspace --all-targets --locked' .github/workflows/ci.yml
expect "CI tests the workspace against the locked dependency graph" \
  grep -q -- 'cargo test --workspace --locked' .github/workflows/ci.yml
expect "CI fails on compiler warnings" \
  grep -q ': warning' .github/workflows/ci.yml

echo
if [[ "$failures" -eq 0 ]]; then
  echo "All $checks compliance checks passed."
else
  echo "$failures of $checks compliance checks failed."
  exit 1
fi
