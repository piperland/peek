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

if [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31mMUTATION CHECK FAILED: %s\033[0m\n' "$1" >&2; exit 1; }

[ -f "$BINDING" ] || fail "$BINDING not found"

restore_binding() {
  if [ -f "$BINDING.mutation.bak" ]; then
    mv "$BINDING.mutation.bak" "$BINDING"
    touch "$BINDING"
  fi
  return 0
}
trap restore_binding EXIT

# `mutate <search> <replacement> <test that must fail>`
#
# The whole test binary runs rather than one test, because `cargo test` exits non-zero and
# its last line is a `rerun with` suggestion rather than a verdict.
mutate() {
  local search="$1" replacement="$2" test="$3"

  step "mutation for $test"
  cp "$BINDING" "$BINDING.mutation.bak"

  python3 - "$BINDING" "$search" "$replacement" <<'PY'
import sys
path, search, replacement = sys.argv[1], sys.argv[2], sys.argv[3]
text = open(path, encoding="utf-8").read()
if text.count(search) != 1:
    sys.exit(f"the search string appears {text.count(search)} times, expected exactly 1: {search!r}")
open(path, "w", encoding="utf-8").write(text.replace(search, replacement))
PY

  local output
  output="$(cargo test --test language_gate -- --test-threads=1 "$test" 2>&1 || true)"

  # Restore before deciding anything, so a failure below cannot leave the tree mutated.
  # The trap is the backstop if this script is interrupted.
  #
  # **Then touch the file.** `mv` preserves the original modification time, and Cargo
  # decides what to rebuild from mtime — so a restore that does not touch leaves a binary
  # built from the mutated source in `target/`, and the next run measures the mutation
  # while reading the clean source. That has already cost three rounds on the sibling
  # script; it is written down here so it is not paid twice.
  restore_binding
  touch "$BINDING"

  if ! printf '%s' "$output" | grep -q '^test result:'; then
    printf '%s\n' "$output"
    fail "$test did not run: the mutation does not compile, so the result is unknown rather \
than a pass"
  fi
  if ! printf '%s' "$output" | grep -q '^test gate::.*FAILED'; then
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

mutate \
  'const ENTITYLESS_BINDERS: &[&str] = &["let_declaration", "for_expression", "closure_expression"];' \
  'const ENTITYLESS_BINDERS: &[&str] = &[];' \
  'a_binding_is_classified_entityless_at_least_once'

mutate \
  '                    .flatten(),
                introduces: introduces.is_some(),' \
  '                    .flatten()
                    .or_else(|| has_ancestor_of_kind(node, "block").then(|| "block".to_owned())),
                introduces: introduces.is_some(),' \
  'the_binding_rule_damages_no_relation_the_fixture_gives_a_referent_for'

mutate \
  'in_body: has_ancestor_of_kind(node, "block"),' \
  'in_body: introduces.is_some(),' \
  'the_positional_rule_damages_something_and_that_is_why_it_was_rejected'

restore_binding
trap - EXIT

step 'the tree is clean again, and so is the build'
if ! git diff --quiet -- "$BINDING"; then
  fail "the measurement was left modified; the mutations were not reverted"
fi
AFTER="$(cargo test --test language_gate -- --test-threads=1 2>&1 | awk '/^test result:/ { print }' | head -1)"
printf '  %s\n' "$AFTER"
case "$AFTER" in
  *FAILED*) fail "after restoring the measurement the tests are red; a mutated build survived the restore" ;;
  *) ;;
esac

printf '\n\033[1;32mBINDING MEASUREMENT MUTATION CHECK OK\033[0m\n'
printf 'Each of the four claims the binding measurement makes failed when the code\n'
printf 'behind it was broken. The join over the whole population is not asserted\n'
printf 'here: it was observed failing, on a relation whose span holds a\n'
printf 'scoped_identifier rather than an identifier, and fixing the measurement to\n'
printf 'join every row meant reading the reference rule out of the specification\n'
printf 'instead of listing the node types again.\n'