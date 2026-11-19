#!/usr/bin/env bash
# Removes untracked files from this Git checkout so a build, an archive or a pull request cannot
# pick up leftovers from earlier experiments.
#
# Guarantees:
#   * tracked files are never touched (`git clean` cannot modify tracked paths)
#   * ignored paths (`node_modules/`, `target/`, `dist/`, …) are kept unless --include-ignored is set
#   * nothing is deleted until --force is passed: the default is a dry run that only lists paths
#
# Usage:
#   scripts/clean-untracked.sh [--force] [--include-ignored]
#
#   (no flags)          list every untracked path Git would remove, delete nothing
#   --force, -f         remove the listed untracked, non-ignored paths
#   --include-ignored   also treat ignored paths as candidates (removes caches and build output)
#   --help, -h          print this help
set -euo pipefail

force=0
include_ignored=0

usage() {
  cat <<'USAGE'
Usage: scripts/clean-untracked.sh [--force] [--include-ignored]

  (no flags)          list every untracked path Git would remove, delete nothing
  --force, -f         remove the listed untracked, non-ignored paths
  --include-ignored   also treat ignored paths as candidates (removes caches and build output)
  --help, -h          print this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --force | -f)
      force=1
      shift
      ;;
    --include-ignored)
      include_ignored=1
      shift
      ;;
    --dry-run)
      force=0
      shift
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown option $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
  echo "Error: this is not inside a Git work tree." >&2
  exit 1
fi
cd "$root"

if ! git rev-parse --verify --quiet HEAD >/dev/null; then
  echo "Error: $root has no commits yet; refusing to clean an empty repository." >&2
  exit 1
fi

clean_args=(-d)
if [[ "$include_ignored" -eq 1 ]]; then
  clean_args+=(-x)
fi

candidates=()
while IFS= read -r path; do
  [[ -n "$path" ]] && candidates+=("$path")
done < <(git clean -n "${clean_args[@]}" | sed 's/^Would remove //')

if [[ "${#candidates[@]}" -eq 0 ]]; then
  echo "Nothing to clean: $root has no untracked paths to remove."
  exit 0
fi

echo "Untracked paths Git would remove (${#candidates[@]}):"
printf '  %s\n' "${candidates[@]}"

if [[ "$force" -eq 0 ]]; then
  echo
  echo "Dry run: nothing was deleted. Re-run with --force to remove these paths."
  exit 0
fi

if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo
  echo "Note: staged and unstaged changes to tracked files are preserved; only untracked paths are removed."
fi

git clean -f "${clean_args[@]}"
echo "Removed ${#candidates[@]} untracked path(s) from $root."
