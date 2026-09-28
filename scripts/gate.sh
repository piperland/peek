#!/usr/bin/env bash
# Piper Peek — deterministic verification gate.
#
# This is the single definition of "the gate". CI calls it; local iteration calls it inside an
# approved Linux environment. It fails loudly and never converts a failure into a warning.
#
# Usage:
#   ./scripts/gate.sh            # full gate: fmt + clippy + test + build
#   ./scripts/gate.sh fmt        # cargo fmt --check
#   ./scripts/gate.sh clippy     # clippy -D warnings
#   ./scripts/gate.sh test       # full test suite
#   ./scripts/gate.sh build      # release build
#   ./scripts/gate.sh quick      # fmt + clippy only

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

# Keep the toolchain on PATH even when invoked without a login shell.
if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mGATE FAILED: %s\033[0m\n' "$1" >&2; exit 1; }

require_cmd() { command -v "$1" >/dev/null 2>&1 || fail "missing required tool: $1"; }

require_cmd cargo
require_cmd git

step "environment"
rustc --version
cargo --version
echo "root: $ROOT"

TARGET="${1:-full}"

case "$TARGET" in
  fmt)
    step "cargo fmt --check"
    cargo fmt --all --check
    ;;
  clippy)
    step "cargo clippy --all-targets --all-features -- -D warnings"
    cargo clippy --all-targets --all-features --locked -- -D warnings
    ;;
  test)
    step "cargo test --all-targets --all-features"
    cargo test --all-targets --all-features --locked
    ;;
  build)
    step "cargo build --release"
    cargo build --release --locked
    ;;
  quick)
    step "cargo fmt --check"
    cargo fmt --all --check
    step "cargo clippy --all-targets --all-features -- -D warnings"
    cargo clippy --all-targets --all-features --locked -- -D warnings
    ;;
  full)
    bash "$ROOT/scripts/gate.sh" fmt
    bash "$ROOT/scripts/gate.sh" clippy
    bash "$ROOT/scripts/gate.sh" test
    bash "$ROOT/scripts/gate.sh" build
    ;;
  *)
    fail "unknown gate target: $TARGET (expected: fmt|clippy|test|build|quick|full)"
    ;;
esac

printf '\n\033[1;32mGATE OK (%s)\033[0m\n' "$TARGET"
