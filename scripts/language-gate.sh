#!/usr/bin/env bash
# The per-language gate: measure, report, and regenerate the published matrix.
#
# Runs `crates/peek-core/tests/language_gate.rs` and prints what it measured. The
# gate is the only thing that writes `LANGUAGE_MATRIX.md` and
# `docs/language-matrix.json`, and it writes them only when asked — so a plain run
# never rewrites a committed file, and a stale published number fails a test rather
# than sitting there looking true.
#
# Usage:
#   ./scripts/language-gate.sh              # measure and report
#   ./scripts/language-gate.sh --write      # measure and regenerate the matrix
#   ./scripts/language-gate.sh --dump       # also print every indexed row
#
# `--dump` is for working out why a number is what it is. The gate prints the
# labels it could not account for and the relations no label claims either, which
# answers most of it; the dump answers the rest by listing every entity and
# relation row the index holds.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

WRITE=""
DUMP=""
for argument in "$@"; do
  case "$argument" in
    --write) WRITE=1 ;;
    --dump) DUMP=1 ;;
    *)
      printf 'unknown option: %s (expected --write or --dump)\n' "$argument" >&2
      exit 2
      ;;
  esac
done

[ -n "$WRITE" ] && export PEEK_GATE_WRITE=1
[ -n "$DUMP" ] && export PEEK_GATE_DUMP=1

cargo test --test language_gate -- --nocapture --test-threads=1

printf '\n'
if [ -n "$WRITE" ]; then
  printf 'regenerated LANGUAGE_MATRIX.md and docs/language-matrix.json\n'
  printf 'review the diff before committing: a matrix is a published claim\n'
else
  printf 'LANGUAGE_MATRIX.md and docs/language-matrix.json were not written.\n'
  printf 'If the numbers moved, run this with --write and commit the result.\n'
fi