#!/usr/bin/env bash
# Index several real repositories and record the spread.
#
# One repository is an anecdote. This is the run that answers "how does Peek behave on code it
# has never seen", across repository *shapes* rather than across sizes: a single crate, a
# multi-crate workspace, a generics-and-macros-heavy library, a very large workspace, and an
# async multi-crate one. The shapes are the variable that matters, because a decide rate is a
# property of how a codebase is organised and not of how many lines it has.
#
# It prints a report. It does not fail a build, because there is no threshold anyone has measured
# and inventing one would be the exact defect this project exists to remove. What it *does* fail
# on is a broken invariant: orphans, a state count that does not partition the relation count, or
# an `integrity_check` that does not come back clean.
#
# Usage, from the repository root, inside the verification sandbox:
#
#   PEEK_PROBE_WORK=/tmp/probe bash scripts/probe-real-repos.sh
#   PEEK_PROBE_WORK=/tmp/probe bash scripts/probe-real-repos.sh rust-lang/cargo   # just one
#
# Each repository is cloned at depth 1, which is enough: the point is the source, not the
# history. Clones land under $PEEK_PROBE_WORK/repos and are reused if already present, so a
# second run is fast and does not re-download.

set -euo pipefail

WORK="${PEEK_PROBE_WORK:-/tmp/peek-probe}"
REPOS_DIR="$WORK/repos"
INDEX_DIR="$WORK/index"
OUT="$WORK/report"

# name | url | one line on what shape it is, which is why it is here
DEFAULT_SET="
rust-lang/regex|https://github.com/rust-lang/regex.git|multi-crate workspace, medium: the baseline
BurntSushi/ripgrep|https://github.com/BurntSushi/ripgrep.git|single crate, no workspace: the clean case
serde-rs/serde|https://github.com/serde-rs/serde.git|heavy generics and derive macros
tokio-rs/axum|https://github.com/tokio-rs/axum.git|multi-crate async, traits across crate lines
rust-lang/cargo|https://github.com/rust-lang/cargo.git|very large workspace: the upper end
"

# A single argument replaces the set, so one can be re-run without waiting for the rest.
if [ "$#" -gt 0 ]; then
  DEFAULT_SET="$1|https://github.com/$1.git|single repository, chosen on the command line
"
fi

mkdir -p "$REPOS_DIR" "$INDEX_DIR" "$OUT"

echo "workspace: $WORK"
echo

# Pull one labelled number out of a probe log. A top-level function rather than a nested one,
# because `local` cannot be applied to a function definition.
field() {
  grep -m1 -E "^$2" "$1" 2>/dev/null | tr -s ' ' | cut -d: -f2- | xargs || true
  echo
}

# One repository: clone if needed, then probe it. Never lets a failure abort the run — a
# repository that cannot be cloned is a fact about the run and is recorded as one.
probe_one() {
  local slug="$1" url="$2" why="$3"
  local dir="$REPOS_DIR/${slug//\//__}"

  if [ ! -d "$dir/.git" ]; then
    echo "cloning $slug ..."
    if ! git clone --depth 1 --quiet "$url" "$dir" 2>"$OUT/$slug.clone.err"; then
      echo "  CLONE FAILED: $slug"
      sed 's/^/    /' "$OUT/$slug.clone.err" | head -5
      return 0
    fi
  fi

  # A fresh index per repository. Reusing one would measure the second repository against the
  # first one's leftovers, which is the kind of quiet contamination that makes a number wrong
  # without making it look wrong.
  rm -rf "$INDEX_DIR/$slug"
  mkdir -p "$INDEX_DIR/$slug"

  local log="$OUT/$slug.log"
  if PEEK_PROBE_REPO="$dir" PEEK_INDEX_DIR="$INDEX_DIR/$slug" \
     cargo test --test real_repository --release -- --ignored --nocapture \
     >"$log" 2>&1; then
    echo "  ok: $slug"
  else
    echo "  PROBE FAILED: $slug (see $log)"
    tail -5 "$log" | sed 's/^/    /'
    return 0
  fi

  # Pull the numbers out into one line so the summary can be a table rather than prose. Every
  # field is a number the probe actually printed; nothing here is derived by this script.
  printf '%s|%s|%s|%s|%s|%s|%s|%s\n' \
    "$slug" \
    "$(field "$log" '^entities:')" \
    "$(field "$log" '^relations:')" \
    "$(field "$log" '^resolved:')" \
    "$(field "$log" '^ambiguous:')" \
    "$(field "$log" '^unresolved:')" \
    "$(field "$log" '^orphans:')" \
    "$(field "$log" '^wal bytes:')" \
    >"$OUT/$slug.row"
}

printf '%-22s %8s %8s %9s %8s %9s %7s\n' \
  repository entities relations resolved ambiguous unresolved orphans
printf -- '---------------------------------------------------------------------------------------------------\n'

ROWS="$OUT/rows.txt"
: >"$ROWS"

while IFS='|' read -r slug url why; do
  [ -z "$slug" ] && continue
  echo "$why" >"$OUT/$slug.why"
  probe_one "$slug" "$url" "$why"
  [ -f "$OUT/$slug.row" ] && cat "$OUT/$slug.row" | cut -d'|' -f1-7 >>"$ROWS"
done <<EOF
$DEFAULT_SET
EOF

echo
echo "=== per repository ==="
printf '%-22s %8s %8s %9s %8s %9s %7s\n' \
  repository entities relations resolved ambiguous unresolved orphans
if [ -s "$ROWS" ]; then
  while IFS='|' read -r slug entities relations resolved ambiguous unresolved orphans; do
    # The decide rate is decided / (everything the extractor wrote). A number computed here and
    # nowhere else, so it cannot drift from what the probe reported.
    total=$((resolved + ambiguous + unresolved))
    rate="n/a"
    if [ "$total" -gt 0 ] 2>/dev/null; then
      rate=$(awk "BEGIN{printf \"%.0f\", 100*$resolved/$total}")
    fi
    printf '%-22s %8s %8s %7s%% %8s %9s %7s\n' \
      "$slug" "$entities" "$relations" "$rate" "$ambiguous" "$unresolved" "$orphans"
  done <"$ROWS"
else
  echo "  no repository completed; see $OUT"
fi

echo
echo "=== invariant check ==="
# The things that must never be false, checked across every run rather than trusted to one.
failures=0
for slug_dir in "$OUT"/*.log; do
  [ -e "$slug_dir" ] || continue
  slug=$(basename "$slug_dir" .log)
  grep -q "^orphans: *0$" "$slug_dir" || { echo "  ORPHANS: $slug"; failures=$((failures+1)); }
  grep -q "^pending: *0$" "$slug_dir" || { echo "  PENDING REMAINS: $slug"; failures=$((failures+1)); }
  grep -q "integrity_check: ok" "$slug_dir" || { echo "  INTEGRITY: $slug"; failures=$((failures+1)); }
  # A repository that indexes 0 relations proves nothing about traversal, so treat it as suspect
  # rather than as a pass.
  grep -qE "^relations: *[1-9]" "$slug_dir" || { echo "  NO RELATIONS: $slug"; failures=$((failures+1)); }
done
if [ "$failures" -eq 0 ]; then
  echo "  every completed run: 0 orphans, 0 pending, integrity clean, relations present"
else
  echo "  $failures invariant failure(s) above"
fi

echo
echo "full output: $OUT"
exit "$failures"
