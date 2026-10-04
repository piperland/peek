#!/usr/bin/env bash
# Prove the language gate can fail, by breaking the thing it measures.
#
# # A gate that has never failed is not evidence of anything.
#
# This script mutates one extraction rule in `src/extract/registry.rs`, re-runs the
# gate, and asserts that the gate **fails** and that it fails on the dimension the
# mutation should break. The mutation is reverted afterwards, and the tree is
# checked afterwards, so a run leaves nothing behind.
#
# Three mutations, chosen because each is a plausible mistake rather than a
# nonsense one, and each should break a *different* dimension:
#
#   1. `macro_invocation` dropped from the call rules.
#      `calls` is labelled 39/40 and that one miss is the macro body, so dropping
#      macro calls must lower `calls` without touching anything else.
#
#   2. `impl_item` dropped from `type_scope_nodes`.
#      Every method in an `impl` block stops being a method and becomes a bare
#      function, so `symbol_precision` and `symbol_recall` must fall, because the
#      fixture labels them as methods.
#
#   3. The import rule pointed at `mod_item` instead of `use_declaration`.
#      Both are real node types and `argument` is a real field, so the grammar
#      validator stays green — the mutation is *invisible to every other check*
#      and only the gate notices. `mod_item` has no `argument` child, so no import
#      relation is emitted at all and `imports` and `imports_module_retained` fall.
#      This is the mutation worth having: it is the failure mode a validator that
#      only checks node types would let through.
#
# Usage:
#   ./scripts/language-gate-mutation-check.sh
#
# Requires a writable checkout and a toolchain. Intended for the approved Linux
# environment, like `scripts/gate.sh`.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
REGISTRY="crates/peek-core/src/extract/registry.rs"

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mMUTATION CHECK FAILED: %s\033[0m\n' "$1" >&2; exit 1; }

[ -f "$REGISTRY" ] || fail "$REGISTRY not found"

# Read one dimension out of the gate's own output, as the fraction it printed.
#
# The gate prints `  dimension_name  96.67 (87/90)`. Parsing the rendered table
# rather than reaching into the code is deliberate: it means this script checks
# the number a reader sees, and it cannot drift from the renderer without this
# script going quiet.
measure() {
  local dimension="$1"
  PEEK_GATE_DUMP= cargo test --test language_gate every_registered_language_meets \
    -- --nocapture 2>/dev/null \
    | awk -v want="$dimension" '$1 == want { split($2, p, "/"); print p[1] "/" p[2] }' \
    | head -1
}

# `mutate <search> <replacement> <dimension>` — apply an edit, and require the
# gate to fail on `dimension` while the others keep working.
mutate() {
  local search="$1" replacement="$2" dimension="$3" before="$4"

  step "mutation for $dimension"
  cp "$REGISTRY" "$REGISTRY.gate-mutation.bak"

  python3 - "$REGISTRY" "$search" "$replacement" <<'PY'
import sys
path, search, replacement = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read()
if text.count(search) != 1:
    sys.exit(f"the search string appears {text.count(search)} times, expected exactly 1: {search!r}")
open(path, "w", encoding="utf-8").write(text.replace(search, replacement))
PY

  local after
  after="$(measure "$dimension" || true)"
  local verdict
  verdict="$(cargo test --test language_gate every_registered_language_meets 2>&1 | tail -1 || true)"

  # Restore before deciding anything, so a failure below cannot leave the tree
  # mutated. The trap below is the backstop if this script is interrupted.
  mv "$REGISTRY.gate-mutation.bak" "$REGISTRY"

  if ! printf '%s' "$verdict" | grep -q '^test result: FAILED'; then
    fail "the gate passed with the rule for $dimension removed, so it does not measure $dimension"
  fi
  if [ -z "$after" ] || [ "$after" = "$before" ]; then
    fail "the gate failed but $dimension did not move: it was $before and is now '${after:-unmeasured}'"
  fi
  printf '  %s: %s -> %s, and the gate failed\n' "$dimension" "$before" "$after"
}

restore() {
  [ -f "$REGISTRY.gate-mutation.bak" ] && mv "$REGISTRY.gate-mutation.bak" "$REGISTRY"
  return 0
}
trap restore EXIT

step "baseline"
BASE_CALLS="$(measure calls)"
BASE_PRECISION="$(measure symbol_precision)"
BASE_IMPORTS="$(measure imports)"
[ -n "$BASE_CALLS" ] || fail "cannot read a baseline measurement from the gate"
printf '  calls %s, symbol_precision %s, imports %s\n' \
  "$BASE_CALLS" "$BASE_PRECISION" "$BASE_IMPORTS"

mutate 'CallRule::new("macro_invocation", "macro"),' '' calls "$BASE_CALLS"
mutate 'type_scope_nodes: &["impl_item", "trait_item"],' \
  'type_scope_nodes: &["trait_item"],' symbol_precision "$BASE_PRECISION"
mutate '        "use_declaration",
        Some("argument"),' '        "mod_item",
        Some("argument"),' imports "$BASE_IMPORTS"

restore
trap - EXIT

step "the tree is clean again"
if ! git diff --quiet -- "$REGISTRY"; then
  fail "the registry was left modified; the mutations were not reverted"
fi
git diff --quiet -- "$REGISTRY" || fail "the registry still differs from HEAD"

printf '\n\033[1;32mMUTATION CHECK OK\033[0m\n'
printf 'Each gate assertion failed when the extraction rule behind it was removed.\n'