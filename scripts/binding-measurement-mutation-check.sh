#!/usr/bin/env bash
# Prove the binding measurement can fail, by breaking the thing it measures.
#
# > **A gate that has never failed is not evidence of anything.**
#
# `scripts/language-gate-mutation-check.sh` does this for the published dimensions. This
# does it for the two candidate rules for a local binding, which is a measurement rather
# than a dimension: it publishes no number, so nothing would notice it quietly matching
# nothing, and "the rule under test did no damage" is the exact shape of a result that
# reads as a pass and is an absence.
#
# Three mutations, each a plausible mistake rather than a nonsense one, and each required
# to fail **one named test**:
#
#   1. The entityless binder list emptied.
#      The classifier stops firing. Every count it produces is then a count over an empty
#      population, and its damage count of zero says nothing — which is why
#      `a_binding_is_classified_entityless_at_least_once` exists and why it is asserted
#      separately rather than folded into the damage figure.
#
#   2. The positional rule added as a fallback.
#      "When in doubt, a body is a local." This is the rule the measurement rejected — a
#      name inside a body is local — written as the one plausible way it could creep back
#      in. It must fail
#      `the_binding_rule_damages_no_relation_the_fixture_gives_a_referent_for`, because a
#      rule that unresolves the fixture's field reads and parameters is not admissible
#      however cheap it looks.
#
#   3. The positional test defined as "a binding this occurrence introduces".
#      The mirror image, and the plausible confusion worth naming: it reads like the adopted
#      rule and is not the same thing. It must fail
#      `the_positional_rule_damages_something_and_that_is_why_it_was_rejected`, which is
#      what stops that rejection being forgotten.
#
# Usage:
#   ./scripts/binding-measurement-mutation-check.sh
#
# Requires a writable checkout and a toolchain. Intended for the approved Linux
# environment, like `scripts/gate.sh`.

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
BINDING="crates/peek-core/tests/gate/binding.rs"

# **The other three, and the three are the three things that can make the class lie.** The
# measurement reads the tree itself, the extractor writes the class the index will carry, and
# the grammar validator decides whether the binding table's strings name anything real. A
# mutation that breaks only the measurement proves the measurement is load-bearing and says
# nothing about the engine, which is the half that reaches a stored index.
EXTRACT_BINDINGS="crates/peek-core/src/extract/bindings.rs"
REGISTRY="crates/peek-core/src/extract/registry.rs"
GRAMMAR="crates/peek-core/src/extract/grammar.rs"

for required in "$BINDING" "$EXTRACT_BINDINGS" "$REGISTRY" "$GRAMMAR"; do
  [ -f "$required" ] || fail "$required not found"
done

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mMUTATION CHECK FAILED: %s\033[0m\n' "$1" >&2; exit 1; }

for required in "$BINDING" "$EXTRACT_BINDINGS" "$REGISTRY" "$GRAMMAR"; do
  [ -f "$required" ] || fail "$required not found"
done

restore_files() {
  for file in "$BINDING" "$EXTRACT_BINDINGS" "$REGISTRY" "$GRAMMAR"; do
    if [ -f "$file.mutation.bak" ]; then
      mv "$file.mutation.bak" "$file"
      touch "$file"
    fi
  done
  return 0
}
trap restore_files EXIT

# `mutate <file> <search> <replacement> <test that must fail> [cargo target]`
#
# The whole test binary runs rather than one test, because `cargo test` exits non-zero and
# its last line is a `rerun with` suggestion rather than a verdict.
#
# The target defaults to the gate, because that is where most of the claims live. The two
# checks that live in the library name their own: a test in one binary cannot observe a
# mutation made on behalf of another.
mutate() {
  local file="$1" search="$2" replacement="$3" test="$4" target="${5:---test language_gate}"

  step "mutation for $test"
  [ -f "$file" ] || fail "$file not found"
  cp "$file" "$file.mutation.bak"

  python3 - "$file" "$search" "$replacement" <<'PY'
import sys
path, search, replacement = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read()
if text.count(search) != 1:
    sys.exit(f"the search string appears {text.count(search)} times, expected exactly 1: {search!r}")
open(path, "w", encoding="utf-8").write(text.replace(search, replacement))
PY

  local output
  output="$(cargo test $target -- --test-threads=1 "$test" 2>&1 || true)"

  # Restore before deciding anything, so a failure below cannot leave the tree mutated.
  # The trap is the backstop if this script is interrupted.
  #
  # **Then touch the file.** `mv` preserves the original modification time, and Cargo
  # decides what to rebuild from mtime — so a restore that does not touch leaves a binary
  # built from the mutated source in `target/`, and the next run measures the mutation
  # while reading the clean source. That has already cost three rounds on the sibling
  # script; it is written down here so it is not paid twice.
  restore_files

  if ! printf '%s' "$output" | grep -q '^test result:'; then
    printf '%s\n' "$output"
    fail "$test did not run: the mutation does not compile, so the result is unknown rather \
than a pass"
  fi
  if ! printf '%s' "$output" | grep -Eq "^test .*${test} .*FAILED"; then
    printf '%s\n' "$output"
    fail "$test passed with the measurement it checks broken, so it does not measure it"
  fi
  printf '  %s failed, as it must\n' "$test"
}

step 'baseline: every binding test green'
BASE="$(cargo test --test language_gate -- --test-threads=1 2>&1 | awk '/^test result:/ { print }' | head -1)"
printf '  %s\n' "$BASE"
case "$BASE" in
  *FAILED*) fail "the binding tests are red before any mutation" ;;
  *) ;;
esac

mutate "$BINDING" \
  'const ENTITYLESS_BINDERS: &[&str] = &["let_declaration", "for_expression", "closure_expression"];' \
  'const ENTITYLESS_BINDERS: &[&str] = &[];' \
  'a_binding_is_classified_entityless_at_least_once'

mutate "$BINDING" \
  '                    .flatten(),
                introduces: introduces.is_some(),' \
  '                    .flatten()
                    .or_else(|| has_ancestor_of_kind(node, "block").then(|| "block".to_owned())),
                introduces: introduces.is_some(),' \
  'the_binding_rule_damages_no_relation_the_fixture_gives_a_referent_for'

mutate "$BINDING" \
  'in_body: has_ancestor_of_kind(node, "block"),' \
  'in_body: introduces.is_some(),' \
  'the_positional_rule_damages_something_and_that_is_why_it_was_rejected'

# The three below break the **engine**, not the measurement. Without them this script proves
# only that a second implementation of the reading is load-bearing, which is not the claim.
# The claim is that the class the index carries is the class the reading produced.

# The binding table itself. Emptied, the classifier fires never and "it damaged nothing" is
# an absence.
mutate "$REGISTRY" \
  '    bindings: &[
        BindingRule::new("let_declaration", "pattern", Some("value")),
        BindingRule::new("for_expression", "pattern", Some("value")),
        BindingRule::new("closure_expression", "parameters", Some("return_type")),
    ],' \
  '    bindings: &[],' \
  'the_extractor_writes_the_local_binding_class_and_damages_nothing'

# Clause 3: the occurrence that writes the binding. Every `binds_nothing` row of this shape is
# local only because of it.
mutate "$EXTRACT_BINDINGS" \
  '    introduces(spec, node, source, name).or_else(|| bound_around(spec, node, source, name))' \
  '    bound_around(spec, node, source, name)' \
  'the_class_reaches_every_relation_the_fixture_says_binds_nothing'

# Clause 1: a name in a parent's `field` slot, in a source where the same name is *also* a
# local in scope. Deliberately **not** checked through the gate.
#
# `member_fields: &[]` was first run against the gate and passed every test there, which is a
# finding rather than a nuisance: no labelled row in the fixture has a name that is both a
# member and a local in scope, so the fixture cannot fail the clause and R-021's claim that
# dropping it destroys four correct edges is not reproduced by this fixture. Pinning it needs
# a source the fixture does not have, and adding one would move `references` and
# `resolution_correctness` for an unrelated change — so the check lives beside the classifier
# and the gate's silence is recorded rather than papered over.
mutate "$REGISTRY" \
  '        member_fields: &["field"],' \
  '        member_fields: &[],' \
  'a_field_name_is_not_classified_by_a_binder_that_binds_that_name' '--lib'

# The enumeration, and not the classifier. Field ids run `1..=count`, so walking `0..count`
# drops the highest-numbered field of every grammar; in `tree-sitter-rust` that is `value`,
# which both a `let` and a `for` use. The check that catches it lives in the library, so it is
# exercised through the library.
mutate "$GRAMMAR" \
  '        for id in 1..=field_count {' \
  '        for id in 0..field_count {' \
  'every_field_the_grammar_has_is_enumerated' '--lib'

restore_files
trap - EXIT

step 'the tree is clean again, and so is the build'
if ! git diff --quiet -- "$BINDING" "$EXTRACT_BINDINGS" "$REGISTRY" "$GRAMMAR"; then
  fail "the measurement was left modified; the mutations were not reverted"
fi
AFTER="$(cargo test --test language_gate -- --test-threads=1 2>&1 | awk '/^test result:/ { print }' | head -1)"
printf '  %s\n' "$AFTER"
case "$AFTER" in
  *FAILED*) fail "after restoring the measurement the tests are red; a mutated build survived the restore" ;;
  *) ;;
esac

printf '\n\033[1;32mBINDING MEASUREMENT MUTATION CHECK OK\033[0m\n'
printf 'Each of the seven claims failed when the code behind it was broken: three\n'
printf 'in the measurement that prices the two candidate rules, three in the extractor\n'
printf 'that writes the class the index carries, and one in the grammar validator that\n'
printf 'keeps the binding table honest.\n'
printf '\n'
printf 'The join over the whole population is not asserted here: it was observed\n'
printf 'failing, on a relation whose span holds a scoped_identifier rather than an\n'
printf 'identifier, and fixing the measurement to join every row meant reading the\n'
printf 'reference rule out of the specification instead of listing the node types\n'
printf 'again.\n'