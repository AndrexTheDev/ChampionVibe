#!/usr/bin/env bash
# Commits a single generated file back to the branch the workflow runs on.
#
# Used by .github/workflows/model-pins.yml to hand artefacts that CI computes (Cargo.lock,
# model-pin discovery reports) back to the repository, because Actions logs and artifacts are
# not always available to the maintainer's environment.
#
# Pushes made with the workflow GITHUB_TOKEN do not trigger further workflow runs, so this
# cannot create a build loop.
set -euo pipefail

path="${1:-}"
message="${2:-}"
if [[ -z "$path" || -z "$message" ]]; then
  echo "usage: scripts/ci-commit-file.sh <path> <commit message>" >&2
  exit 2
fi
if [[ ! -f "$path" ]]; then
  echo "Error: $path does not exist, so there is nothing to commit." >&2
  exit 1
fi

branch="${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD)}"
if [[ -z "$branch" || "$branch" == "HEAD" ]]; then
  echo "Error: cannot determine the branch to commit to." >&2
  exit 1
fi

git config user.name "championvibe-ci"
git config user.email "ci@users.noreply.github.com"
git add "$path"

if git diff --cached --quiet; then
  echo "No change in $path; nothing to commit."
  exit 0
fi

git commit -m "$message"
git fetch origin "$branch"
git rebase "origin/$branch"
git push origin "HEAD:$branch"
echo "Committed and pushed $path to $branch."
