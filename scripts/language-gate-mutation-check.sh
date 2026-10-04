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
#   4. The reference rule's node types emptied.
#      `references` is the one dimension whose score was produced by a rule rather
#      than by a grammar, and it read `0.00 (0/18)` for a long time — not because
#      references are rare but because `is_excluded_reference` dropped every
#      identifier inside a declaration. An empty list is the same shape of mistake
#      written the other way round, and it is the only mutation that proves the
#      gate notices when the `References` class stops producing. A gate that cannot
#      fail on a dimension is not measuring it.
#
#   5. `scoped_identifier` dropped from the reference rule's node types.
#      Every other mutation breaks extraction as well as resolution, so none of them
#      can tell the new column from the ones already in the table. This one emits
#      **one fewer reference row per qualified path** and changes nothing else:
#      `service::describe` was a row nobody could place, and it is gone. The
#      undecided count falls and `resolution_correctness` moves, because the
#      denominator is the decided relations and dropping an unplaceable row changes
#      which ones remain. It is the mutation that shows the new column is computed
#      from the graph rather than from a number in a file.
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
  # `cargo test` writes two kinds of line to stdout. The measurement table is
  # indented by exactly two spaces and the dimension is followed by a rendered
  # figure; `Running`, `Compiling` and `test gate::…` are not indented that way, so
  # anchoring on `^  <name> ` picks the table row and nothing else. An earlier
  # version matched the bare name and picked up the relation dump's
  # `  calls from …` rows, which read as a fraction of `from/`.
  unset PEEK_GATE_DUMP
  cargo test --test language_gate every_registered_language_meets \
    -- --nocapture 2>/dev/null \
    | awk -v want="  $dimension " '$0 ~ "^" want { print $2 }' \
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
  # The whole run, not one test: `cargo test` exits non-zero and its last line is
  # a `rerun with` suggestion rather than a verdict, so the verdict is read from
  # the test binary's own summary.
  verdict="$(cargo test --test language_gate 2>&1 \
    | awk '/^test result:/ { print }' | head -1 || true)"

  # Restore before deciding anything, so a failure below cannot leave the tree
  # mutated. The trap below is the backstop if this script is interrupted.
  #
  # **Then touch the file.** `mv` preserves the *original* modification time, and
  # Cargo decides what to rebuild from mtime — so a restore that does not touch
  # leaves a binary built from the mutated source in `target/`, and the next run
  # measures the mutation while reading the clean source. That happened here: three
  # runs in a row reported `calls` at 38/40 against a source that declares a macro
  # call rule, and the cause was a stale test binary rather than the engine.
  restore_registry
  touch "$REGISTRY"

  if [ -z "$verdict" ]; then
    fail "the gate produced no test summary under the mutation for $dimension; the build is \
broken rather than the measurement having moved"
  fi
  if ! printf '%s' "$verdict" | grep -q 'FAILED'; then
    fail "the gate passed with the rule for $dimension removed, so it does not measure $dimension \
($verdict)"
  fi
  if [ -z "$after" ] || [ "$after" = "$before" ]; then
    fail "the gate failed but $dimension did not move: it was $before and is now '${after:-unmeasured}'"
  fi
  printf '  %s: %s -> %s, and the gate failed\n' "$dimension" "$before" "$after"
}

restore_registry() {
  if [ -f "$REGISTRY.gate-mutation.bak" ]; then
    mv "$REGISTRY.gate-mutation.bak" "$REGISTRY"
    touch "$REGISTRY"
  fi
  return 0
}
trap restore_registry EXIT

step "baseline"
BASE_CALLS="$(measure calls)"
BASE_PRECISION="$(measure symbol_precision)"
BASE_IMPORTS="$(measure imports)"
BASE_REFERENCES="$(measure references)"
BASE_PLACEMENT="$(measure resolution_correctness)"
for pair in "calls:$BASE_CALLS" "symbol_precision:$BASE_PRECISION" \
  "imports:$BASE_IMPORTS" "references:$BASE_REFERENCES" \
  "resolution_correctness:$BASE_PLACEMENT"; do
  case "$pair" in
    ?*:?*) ;;
    *) fail "cannot read a baseline for ${pair%%:*}; the gate printed nothing for it" ;;
  esac
done
printf '  calls %s, symbol_precision %s, imports %s, references %s, resolution_correctness %s\n' \
  "$BASE_CALLS" "$BASE_PRECISION" "$BASE_IMPORTS" "$BASE_REFERENCES" "$BASE_PLACEMENT"

mutate 'CallRule::new("macro_invocation", "macro"),' '' calls "$BASE_CALLS"
mutate 'type_scope_nodes: &["impl_item", "trait_item"],' \
  'type_scope_nodes: &["trait_item"],' symbol_precision "$BASE_PRECISION"
mutate '        "use_declaration",
        Some("argument"),' '        "mod_item",
        Some("argument"),' imports "$BASE_IMPORTS"
mutate '        node_types: &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "scoped_identifier",
        ],' '        node_types: &[],' references "$BASE_REFERENCES"
mutate '            "field_identifier",
            "scoped_identifier",' '            "field_identifier",' resolution_correctness "$BASE_PLACEMENT"

restore_registry
trap - EXIT

step "the tree is clean again, and so is the build"
if ! git diff --quiet -- "$REGISTRY"; then
  fail "the registry was left modified; the mutations were not reverted"
fi
# Rebuild from the restored source and confirm the measurement is the one the
# unmutated engine produces. Without this the script can report "the gate failed
# correctly" while leaving a mutated binary behind for the next run.
AFTER_ALL="$(measure calls)"
if [ "$AFTER_ALL" != "$BASE_CALLS" ]; then
  fail "after restoring the registry, calls is $AFTER_ALL rather than the baseline $BASE_CALLS; \
a mutated build survived the restore"
fi
printf '  calls back at %s, matching the baseline\n' "$AFTER_ALL"

printf '\n\033[1;32mMUTATION CHECK OK\033[0m\n'
printf 'Each gate assertion failed when the extraction rule behind it was removed.\n'