#!/usr/bin/env bash
# ChampionVibe local Rust toolchain bootstrap.
#
# Installs everything a fresh machine needs to build and test this repository the way CI and the
# release pipeline do:
#
#   * the Rust toolchain (cargo, rustc, rustfmt) through rustup,
#   * the cross targets the release pipeline builds: x86_64-pc-windows-msvc and
#     aarch64-linux-android,
#   * the workspace tests with the checked-in Cargo.lock (`cargo test --workspace --locked`).
#
# The script only touches $RUSTUP_HOME, $CARGO_HOME and the repository's target directory, and it is
# safe to re-run: every step checks what is already installed first.
#
# Usage:
#   scripts/bootstrap_rust.sh [--toolchain <channel>] [--skip-targets] [--skip-tests] [--dry-run]
#
# Environment:
#   BOOTSTRAP_RUST_TOOLCHAIN  toolchain channel, default "stable"
#   BOOTSTRAP_RUST_TEST_ARGS  extra arguments for cargo test, e.g. "-p championvibe-issuer"
#
# Notes:
#   * Adding the Android target is not the same as building for Android: an aarch64-linux-android
#     link additionally needs the Android NDK (`ANDROID_NDK_HOME`/`NDK_HOME`) and the tauri CLI.
#     The Windows MSVC target only links on a Windows host or with a cross linker; on Linux and
#     macOS the target is installed so `cargo check --target x86_64-pc-windows-msvc` type-checks.
#   * Installing the toolchain downloads from static.rust-lang.org, which needs network access. On
#     an offline machine, install rustup yourself and re-run this script.

set -euo pipefail

readonly WINDOWS_TARGET="x86_64-pc-windows-msvc"
readonly ANDROID_TARGET="aarch64-linux-android"
readonly RUSTUP_INSTALL_URL="https://sh.rustup.rs"
readonly CARGO_BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"

TOOLCHAIN="${BOOTSTRAP_RUST_TOOLCHAIN:-stable}"
INSTALL_TARGETS=1
RUN_TESTS=1
DRY_RUN=0

usage() {
  cat <<'USAGE'
Bootstrap the Rust toolchain for this repository.

Options:
  --toolchain <channel>  Toolchain channel to install (default: stable)
  --skip-targets         Do not add the Windows MSVC and Android targets
  --skip-tests           Do not run `cargo test --workspace --locked`
  --dry-run              Print the commands instead of running them
  -h, --help             Show this help
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --toolchain)
      [ "$#" -ge 2 ] || { echo "error: --toolchain needs a value" >&2; exit 2; }
      TOOLCHAIN="$2"
      shift 2
      ;;
    --toolchain=*)
      TOOLCHAIN="${1#*=}"
      shift
      ;;
    --skip-targets)
      INSTALL_TARGETS=0
      shift
      ;;
    --skip-tests)
      RUN_TESTS=0
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

say() {
  printf '==> %s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

run() {
  printf '  $ %s\n' "$*"
  if [ "$DRY_RUN" -eq 1 ]; then
    return 0
  fi
  "$@"
}

have() {
  command -v "$1" >/dev/null 2>&1
}

enter_repository_root() {
  local script_dir
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  cd "$script_dir/.."
  if [ ! -f Cargo.toml ]; then
    echo "error: Cargo.toml not found in $(pwd); run this script from a checkout" >&2
    exit 1
  fi
  say "repository: $(pwd)"
}

activate_cargo_home() {
  # A rustup install is only visible after $CARGO_HOME/bin is on PATH.
  if [ -d "$CARGO_BIN_DIR" ]; then
    case ":$PATH:" in
      *":$CARGO_BIN_DIR:"*) ;;
      *) PATH="$CARGO_BIN_DIR:$PATH" ;;
    esac
    export PATH
  fi
  if [ -f "${CARGO_HOME:-$HOME/.cargo}/env" ]; then
    # shellcheck disable=SC1090 # the file is created by rustup and cannot be resolved statically
    . "${CARGO_HOME:-$HOME/.cargo}/env"
  fi
}

install_rustup() {
  activate_cargo_home
  if have rustup && have cargo && have rustc; then
    say "rustup, cargo and rustc are already installed"
    return 0
  fi
  if have cargo && have rustc && ! have rustup; then
    warn "found cargo/rustc without rustup; toolchain and target management will be skipped"
    return 0
  fi
  if ! have curl && [ "$DRY_RUN" -eq 0 ]; then
    echo "error: curl is required to install rustup; install rustup manually and re-run" >&2
    exit 1
  fi
  say "installing rustup and the $TOOLCHAIN toolchain"
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "  \$ curl --proto '=https' --tlsv1.2 -sSf $RUSTUP_INSTALL_URL | sh -s -- -y --default-toolchain $TOOLCHAIN --profile minimal --component rustfmt"
    return 0
  fi
  curl --proto '=https' --tlsv1.2 -sSf "$RUSTUP_INSTALL_URL" |
    sh -s -- -y --default-toolchain "$TOOLCHAIN" --profile minimal --component rustfmt
  activate_cargo_home
  if ! have rustup; then
    echo "error: rustup is still not on PATH; add $CARGO_BIN_DIR to PATH and re-run" >&2
    exit 1
  fi
}

ensure_toolchain() {
  if ! have rustup; then
    return 0
  fi
  say "ensuring the $TOOLCHAIN toolchain with rustfmt"
  run rustup toolchain install "$TOOLCHAIN" --profile minimal --component rustfmt
  # A toolchain installed by rustup becomes active by default; keep an existing default intact.
  if [ "$DRY_RUN" -eq 0 ] && ! rustup default >/dev/null 2>&1; then
    run rustup default "$TOOLCHAIN"
  fi
}

ensure_targets() {
  if [ "$INSTALL_TARGETS" -eq 0 ]; then
    say "skipping the cross targets"
    return 0
  fi
  if ! have rustup; then
    warn "rustup is unavailable, so $WINDOWS_TARGET and $ANDROID_TARGET were not added"
    return 0
  fi
  say "adding the release targets: $WINDOWS_TARGET and $ANDROID_TARGET"
  run rustup target add --toolchain "$TOOLCHAIN" "$WINDOWS_TARGET" "$ANDROID_TARGET"
  if [ "$DRY_RUN" -eq 0 ]; then
    local installed
    installed="$(rustup target list --toolchain "$TOOLCHAIN" --installed || true)"
    for target in "$WINDOWS_TARGET" "$ANDROID_TARGET"; do
      if ! printf '%s\n' "$installed" | grep -qx "$target"; then
        warn "$target is not installed; cross builds for it will fail until it is"
      fi
    done
  fi
}

verify_tooling() {
  say "toolchain versions"
  if [ "$DRY_RUN" -eq 0 ]; then
    cargo --version
    rustc --version
    rustfmt --version
  fi
}

run_tests() {
  if [ "$RUN_TESTS" -eq 0 ]; then
    say "skipping the workspace tests"
    return 0
  fi
  say "running the workspace tests with the checked-in Cargo.lock"
  # shellcheck disable=SC2086 # word splitting is intentional: the args come from the caller
  run cargo test --workspace --locked ${BOOTSTRAP_RUST_TEST_ARGS:-}
}

main() {
  enter_repository_root
  install_rustup
  ensure_toolchain
  ensure_targets
  verify_tooling
  run_tests
  if [ "$DRY_RUN" -eq 1 ]; then
    say "dry run: nothing was installed and no tests were executed"
  else
    say "done: cargo, rustc, rustfmt and the release targets are available"
  fi
}

main "$@"
