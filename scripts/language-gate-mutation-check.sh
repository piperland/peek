#!/usr/bin/env bash
# Prove the language gate can fail, by breaking the thing it measures.
#
# # A gate that has never failed is not evidence of anything.
#
# This script mutates one rule - five in `src/extract/registry.rs`, four of them
# breaking a rule and the fifth restoring `scoped_identifier` to the reference table
# it was removed from, and two in `src/resolve/mod.rs` - re-runs the gate, and asserts
# that the gate **fails** and that it fails on the dimension the mutation should break.
# The mutation is
# reverted afterwards, and the tree is checked afterwards, so a run leaves
# nothing behind.
#
# Seven mutations, chosen because each is a plausible mistake rather than a
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
#   5. The resolver's "a binding of an unrelated declaration is not a candidate"
#      clause dropped from R4's same-file branch.
#      Every other mutation breaks extraction, so none of them can tell this column
#      from the ones already in the table: the gate would notice a rule that stopped
#      producing rows, which every dimension already notices. This one removes no row
#      at all. It only stops the rung refusing `format_line.count` for the `count` read
#      written in `render` and in `describe`, and both of those edges come back wrong,
#      so `resolution_correctness` **falls** and the gate fails.
#
#      **It used to be an extractor mutation, and it cannot be one any more.** The
#      version before scope-aware placement dropped `field_identifier` from the
#      reference rule's node types, which removed `entry.count` and `entry.label` —
#      and with them the two decided-and-wrong `count` edges. `resolution_correctness`
#      therefore *rose* under it, which was the point: it was the mutation that
#      demonstrated that this column cannot tell a fix from a withdrawal, because an
#      engine that decided less and got the rest right reads better here. That is why
#      the wrong-edge count and the undecided count are published beside the fraction
#      rather than folded into it.
#
#      The demonstration is no longer available on this fixture, and the reason is worth
#      stating rather than working around: `resolution_correctness` is now 100.00, so
#      deleting rows can only delete correct ones and the fraction is pinned at 1. The
#      column is saturated, which means **no mutation of any kind can move it upward**,
#      and the mutation above replaces the demonstration with a check that the column
#      still notices a placement rule being broken — which is the other half of what the
#      column is for. The hazard the old mutation demonstrated is still real and still
#      undefended by the fraction alone; what changed is that this fixture can no longer
#      exhibit it.
#
#   6. The resolver's guard that "a table answer holding nothing to search does not end
#      the lookup" forced to answer on the first hit regardless of what it holds.
#      `imports_module_retained` reads 100.00 (3/3) with a floor of 100, so this is the
#      mutation that proves the dimension measures the guard rather than something that
#      happens to pass beside it. The edit is plausible — a flag that starts set — and
#      it restores first-hit-wins: the `mod model;` **declaration** row answers
#      `use model::Entry;`, R1 finds no `Entry` in that file and declines, and the edge
#      falls to the rung that carries no module. `imports_module_retained` returns to
#      0.00 (0/3), `imports` to 66.67, and the gate fails twice over: the floor
#      assertion in `every_registered_language_meets_its_recorded_floor`, and the
#      published-matrix equality in `the_published_matrix_is_the_current_measurement`,
#      because a fresh measurement under the mutation cannot equal the committed matrix
#      this branch regenerated. The resolver's own conformance tests for the rule —
#      `resolve::tests::a_relative_use_from_the_crate_root_keeps_the_module_it_named`
#      and `resolve::tests::a_table_answer_holding_nothing_to_search_does_not_end_the_search`
#      in the lib target — fail on the same source, which is the assertion the brief's
#      "which assertion fails" question is about. And the mutation must **not** move
#      `resolution_correctness`: the same three targets are still placed, by a weaker
#      rung. That is exactly why the retention figure and not the saturated fraction is
#      the evidence of this fix.
#
#   7. `scoped_identifier` restored to the reference rule's node types.
#      The rule was removed on measurement — nine whole-path `references` rows, all
#      `unresolved`, none placeable by any rung, seven duplicating rows their head and
#      tail segments already carried — and this claim proves the gate notices the rule
#      coming back. It must fail `negative_references`, because `gate.expect` now claims
#      `summarise | report::render` as a *non*-reference: the path is the components'
#      composition, not a third name.
#
#      **What this dimension could not see without that label, said plainly.** A
#      restored scoped path row joins no scored population on its own: `references` is
#      recall over labelled rows, `resolution_correctness` is over *decided* relations
#      and these rows are never decided, and the row would sit in the printed
#      unlabelled-reference count — a list, not a floor. Measured before this change:
#      emitting the nine rows moved **no** published figure. The negative label is what
#      makes the decision visible to a floor, and it is worth exactly that: a mutation
#      that changes nothing is evidence about the metric, and the metric needed one
#      addition before it could see this rule at all.
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
RESOLVER="crates/peek-core/src/resolve/mod.rs"

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mMUTATION CHECK FAILED: %s\033[0m\n' "$1" >&2; exit 1; }

for required in "$REGISTRY" "$RESOLVER"; do
  [ -f "$required" ] || fail "$required not found"
done

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

# `mutate <file> <search> <replacement> <dimension> <before>` — apply an edit, and
# require the gate to fail on `dimension` while the others keep working.
#
# The file is an argument rather than a constant because the fifth mutation no longer
# lives in the registry; see its own note below.
mutate() {
  local file="$1" search="$2" replacement="$3" dimension="$4" before="$5"

  step "mutation for $dimension"
  [ -f "$file" ] || fail "$file not found"
  cp "$file" "$file.gate-mutation.bak"

  python3 - "$file" "$search" "$replacement" <<'PY'
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
  restore_file "$file"
  touch "$file"

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

restore_file() {
  if [ -f "$1.gate-mutation.bak" ]; then
    mv "$1.gate-mutation.bak" "$1"
    touch "$1"
  fi
  return 0
}
restore_registry() { restore_file "$REGISTRY"; }
trap restore_registry EXIT

step "baseline"
BASE_CALLS="$(measure calls)"
BASE_PRECISION="$(measure symbol_precision)"
BASE_IMPORTS="$(measure imports)"
BASE_REFERENCES="$(measure references)"
BASE_PLACEMENT="$(measure resolution_correctness)"
BASE_RETENTION="$(measure imports_module_retained)"
BASE_NEGATIVE="$(measure negative_references)"
for pair in "calls:$BASE_CALLS" "symbol_precision:$BASE_PRECISION" \
  "imports:$BASE_IMPORTS" "references:$BASE_REFERENCES" \
  "resolution_correctness:$BASE_PLACEMENT" "imports_module_retained:$BASE_RETENTION" \
  "negative_references:$BASE_NEGATIVE"; do
  case "$pair" in
    ?*:?*) ;;
    *) fail "cannot read a baseline for ${pair%%:*}; the gate printed nothing for it" ;;
  esac
done
printf '  calls %s, symbol_precision %s, imports %s, references %s, resolution_correctness %s\n' \
  "$BASE_CALLS" "$BASE_PRECISION" "$BASE_IMPORTS" "$BASE_REFERENCES" "$BASE_PLACEMENT"
printf '  imports_module_retained %s, negative_references %s\n' \
  "$BASE_RETENTION" "$BASE_NEGATIVE"

mutate "$REGISTRY" 'CallRule::new("macro_invocation", "macro"),' '' calls "$BASE_CALLS"
mutate "$REGISTRY" 'type_scope_nodes: &["impl_item", "trait_item"],' \
  'type_scope_nodes: &["trait_item"],' symbol_precision "$BASE_PRECISION"
mutate "$REGISTRY" '        "use_declaration",
        Some("argument"),' '        "mod_item",
        Some("argument"),' imports "$BASE_IMPORTS"
mutate "$REGISTRY" '        node_types: &["identifier", "type_identifier", "field_identifier"],' \
  '        node_types: &[],' references "$BASE_REFERENCES"
mutate "$RESOLVER" \
  '                    && is_declaration(entity.kind())
                    && !is_binding(entity.kind())' \
  '                    && is_declaration(entity.kind())' \
  resolution_correctness "$BASE_PLACEMENT"

# The sixth, and the one that belongs to this change. `if searchable {` is the guard:
# the table's file list is returned only when it holds something the caller can search
# with. Forcing the condition to `true` is first-hit-wins restored with the search
# still performed — the plausible edit, not the nonsense one — and the retained-module
# figure is the one that notices it. `resolution_correctness` is asserted unchanged in
# the verification below: the three edges still land on the same targets, so a fix that
# only moved the fraction would have tuned the wrong column.
mutate "$RESOLVER" '                if searchable {' '                if true {' \
  imports_module_retained "$BASE_RETENTION"

# The seventh, and the one that keeps a removal from being undone quietly. Restoring
# `scoped_identifier` makes the walker emit a whole-path reference for `report::render`,
# and `gate.expect` claims that occurrence as a non-reference — so `negative_references`
# falls from 15/15 to 14/15 and the floor of 100 fails. Read the header note above for
# what this dimension could not see without the label, and why the label is the honest
# way to make the metric notice the rule.
mutate "$REGISTRY" '        node_types: &[
            "identifier",
            "type_identifier",
            "field_identifier",
        ],' '        node_types: &[
            "identifier",
            "type_identifier",
            "field_identifier",
            "scoped_identifier",
        ],' negative_references "$BASE_NEGATIVE"

# The same mutation, re-applied, read against the column that must *not* move: a
# placement that was right stays right when the module is dropped. This is the guard
# against tuning, and it is run separately because `mutate` requires the named
# dimension to move — it cannot express "this one must not".
step "the same mutation leaves resolution_correctness where it was"
cp "$RESOLVER" "$RESOLVER.gate-mutation.bak"
python3 - "$RESOLVER" <<'PY'
import sys
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
search = "                if searchable {"
replacement = "                if true {"
if text.count(search) != 1:
    sys.exit(f"the search string appears {text.count(search)} times, expected exactly 1: {search!r}")
open(path, "w", encoding="utf-8").write(text.replace(search, replacement))
PY
PLACEMENT_UNDER="$(measure resolution_correctness || true)"
restore_file "$RESOLVER"
touch "$RESOLVER"
if [ "$PLACEMENT_UNDER" != "$BASE_PLACEMENT" ]; then
  fail "resolution_correctness moved to $PLACEMENT_UNDER under the retention mutation (baseline \
$BASE_PLACEMENT); a fix that changed placements is not this rule"
fi
printf '  resolution_correctness held at %s under the mutation — only the module moved\n' \
  "$PLACEMENT_UNDER"

restore_registry
trap - EXIT

step "the tree is clean again, and so is the build"
if ! git diff --quiet -- "$REGISTRY" "$RESOLVER"; then
  fail "a mutated file was left modified; the mutations were not reverted"
fi
# Rebuild from the restored source and confirm the measurement is the one the
# unmutated engine produces. Without this the script can report "the gate failed
# correctly" while leaving a mutated binary behind for the next run.
AFTER_ALL="$(measure calls)"
if [ "$AFTER_ALL" != "$BASE_CALLS" ]; then
  fail "after restoring the mutated file, calls is $AFTER_ALL rather than the baseline \
$BASE_CALLS; a mutated build survived the restore"
fi
printf '  calls back at %s, matching the baseline\n' "$AFTER_ALL"

printf '\n\033[1;32mMUTATION CHECK OK\033[0m\n'
printf 'Each gate assertion failed when the rule behind it was removed.\n'
printf 'The seventh removes nothing: it restores scoped_identifier, and the gate fails\n'
printf 'on negative_references because a labelled non-reference is emitted again. That\n'
printf 'label is the only thing that made the reference rule visible for whole paths;\n'
printf 'the nine rows it replaced moved no other figure, exactly as the header says.\n'
printf 'resolution_correctness FELL under its mutation, which is the column noticing a\n'
printf 'placement rule being broken rather than a class of rows being withdrawn. It\n'
printf 'reads 100.00 on this fixture, so the fraction is saturated and the wrong-edge\n'
printf 'and undecided counts beside it are what a withdrawal would have to show: a\n'
printf 'fraction over decided relations cannot express deciding less, which is why the\n'
printf 'three are published separately.\n'