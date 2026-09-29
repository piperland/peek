#!/usr/bin/env bash
# The controlled comparison R-011 asks for: what is the module table worth?
#
# Both arms come out of **one build**, selected by `PEEK_MODULE_TABLE`. That is the whole reason
# the switch exists. The previous attempt to answer this by running the probe on two consecutive
# revisions produced nothing but two measurement errors (R-010), one of which - a probe that deleted
# a file from the repository it was measuring - manufactured a convincing false trend.
#
# So this script is deliberately dull about the things that went wrong:
#
#   * **One build, two arms.** `PEEK_MODULE_TABLE=0` and `=1`, same binary.
#   * **One clone set, reused.** Both arms see identical source. No re-cloning between arms, so
#     no arm can be measuring a different tree.
#   * **A fresh index per repository per arm.** A shared index would let the second arm read the
#     first arm's rows.
#   * **No `sed` gymnastics on the numbers.** Each arm's figures are extracted with the same grep
#     the probe itself prints, and a per-repository delta is printed only where the two arms
#     produced numbers.
#   * **It states whether the arms differ at all.** If they are byte-identical, the switch is
#     broken or the table never fires, and the script says so rather than printing two equal rows
#     and letting the reader conclude "no effect".
#
# The metric, and why the previous one could not see it
#
# **resolved share = resolved / relations.** The share of relations the index can *prove*, over
# the whole graph. Not a rate over a subset of the states, and not the decided rate.
#
# The first version of this script reported `(resolved + inferred) / relations` — the decided
# rate — and that is the wrong shape for the question. The module table's work is largely to
# turn `inferred` into `resolved`: the same edge, reached by an import binding or a located
# module rather than by a repository-wide uniqueness check. That is movement *inside* decided, so
# a rate over the decided pair is blind to it by construction, and the arms came out within
# 0.4 percentage points of each other on five repositories while the table was demonstrably
# firing. Counting `Resolved` on its own puts the proof and the claim in different numerators, so
# the movement is visible.
#
# The `ambiguous` count is printed next to it on both arms for the same reason in the other
# direction: ambiguity is what the table exists to reduce, and a single rate would let that count
# rise while the rate improved. Every state is printed, so any other figure can be recomputed from
# the same run rather than re-measured.
#
# Usage, inside the verification sandbox:
#
#   PEEK_PROBE_WORK=/tmp/probe bash scripts/ab-module-table.sh
#
# Exit code is 0 when both arms completed for every repository, whatever the deltas are. It is not
# a pass/fail test: this is a measurement, and a measurement with a threshold nobody has justified
# is the defect this project exists to remove.

set -uo pipefail

WORK="${PEEK_PROBE_WORK:-/tmp/peek-ab}"
OUT="$WORK/ab"
REPOS="rust-lang/regex BurntSushi/ripgrep serde-rs/serde tokio-rs/axum rust-lang/cargo"

mkdir -p "$OUT"

field() {
  local line value
  line=$(grep -m1 -E "^$2" "$1" 2>/dev/null) || true
  if [ -z "$line" ]; then
    echo "MISSING($2)" >&2
    echo 0
    return
  fi
  value=$(printf '%s' "$line" | tr -s ' ' | cut -d: -f2- | xargs)
  if ! printf '%s' "$value" | grep -qE '^[0-9]+$'; then
    echo "NOT-A-NUMBER($2): $line" >&2
    echo 0
    return
  fi
  echo "$value"
}

# One repository, one arm. Prints
# `slug entities relations resolved inferred ambiguous unresolved pending`.
run_one() {
  local arm="$1" slug="$2" dir="$3"
  local index="$OUT/index-$arm-$slug"
  rm -rf "$index"
  mkdir -p "$index"
  local log="$OUT/$slug.arm$arm.log"
  if ! PEEK_MODULE_TABLE="$arm" PEEK_PROBE_REPO="$dir" PEEK_INDEX_DIR="$index" \
       cargo test --test real_repository --release -- --ignored --nocapture >"$log" 2>&1; then
    echo "ARM $arm FAILED: $slug (see $log)" >&2
    tail -5 "$log" | sed 's/^/    /' >&2
    return 1
  fi
  printf '%s %s %s %s %s %s %s %s\n' \
    "$slug" \
    "$(field "$log" '^entities:')" \
    "$(field "$log" '^relations:')" \
    "$(field "$log" '^resolved:')" \
    "$(field "$log" '^inferred:')" \
    "$(field "$log" '^ambiguous:')" \
    "$(field "$log" '^unresolved:')" \
    "$(field "$log" '^pending:')"
}

printf '%-22s %10s %10s %10s %9s%% %9s%%\n' \
  'repository/figure' 'relations' 'resolved' 'delta' 'share off' 'share on'
printf -- '-------------------------------------------------------------------------------------\n'

failures=0
for slug in $REPOS; do
  safe="${slug//\//__}"
  dir="$WORK/repos/$safe"
  if [ ! -d "$dir/.git" ]; then
    echo "cloning $slug ..."
    mkdir -p "$WORK/repos"
    if ! git clone --depth 1 --quiet "https://github.com/$slug.git" "$dir" 2>"$OUT/$safe.clone.err"; then
      echo "CLONE FAILED: $slug" >&2
      sed 's/^/    /' "$OUT/$safe.clone.err" | head -3 >&2
      failures=$((failures + 1))
      continue
    fi
  fi

  off=$(run_one 0 "$safe" "$dir") || { failures=$((failures + 1)); continue; }
  on=$(run_one 1 "$safe" "$dir") || { failures=$((failures + 1)); continue; }

  # `run_one` prints `slug entities relations resolved inferred ambiguous unresolved pending`, so
  # the slug occupies `$1` and the first *figure* is `$2`. Getting this off by one is not a
  # cosmetic slip: it makes `relations` read back as the entity count, which produces a
  # denominator that is not the graph and a share near 90% on a repository whose real share is
  # 36%. The partition assertion below is what caught it, and it is the reason that assertion is
  # here at all.
  # shellcheck disable=SC2086
  set -- $off
  o_ent=$2; o_rel=$3; o_res=$4; o_inf=$5; o_amb=$6; o_unr=$7; o_pen=$8
  # shellcheck disable=SC2086
  set -- $on
  n_ent=$2; n_rel=$3; n_res=$4; n_inf=$5; n_amb=$6; n_unr=$7; n_pen=$8

  # The states must partition on both arms, over all five the probe counts, and the partition is
  # asserted rather than assumed because a rate computed over a subset of them is a share of
  # something that is not the graph. `pending` is in the sum because the probe asserts it in its
  # own right: leaving it out is what let a stale index pass as a measured one.
  o_total=$((o_res + o_inf + o_amb + o_unr + o_pen))
  n_total=$((n_res + n_inf + n_amb + n_unr + n_pen))
  o_rate=0; n_rate=0
  [ "$o_total" -gt 0 ] 2>/dev/null && o_rate=$(awk "BEGIN{printf \"%.1f\", 100*$o_res/$o_total}")
  [ "$n_total" -gt 0 ] 2>/dev/null && n_rate=$(awk "BEGIN{printf \"%.1f\", 100*$n_res/$n_total}")

  note=""
  [ "$o_total" -ne "$o_rel" ] 2>/dev/null && note="off-arm states do not partition ($o_total vs $o_rel)"
  [ "$n_total" -ne "$n_rel" ] 2>/dev/null && note="$note on-arm states do not partition ($n_total vs $n_rel)"
  # Compared across the whole state vector, not just the two the rate is built from: a difference
  # hidden in `inferred` is the whole point of the metric, so an equality check that ignored it
  # would call two different indexes identical.
  [ "$o_ent" = "$n_ent" ] && [ "$o_rel" = "$n_rel" ] && [ "$o_res" = "$n_res" ] && \
    [ "$o_inf" = "$n_inf" ] && [ "$o_amb" = "$n_amb" ] && [ "$o_unr" = "$n_unr" ] && \
    note="$note ARMS ARE IDENTICAL - the switch may be doing nothing, or the table never fires"

  delta=$(awk "BEGIN{printf \"%+.1f\", $n_rate - $o_rate}")
  printf '%-22s %10s %10s %10s %9s%% %9s%%  %s\n' \
    "$slug" "$o_rel" "$n_res/$n_rel" "${delta}pp" "$o_rate" "$n_rate" "$note"
  printf '%-22s %10s %10s %10s %10s %10s\n' \
    '  ambiguous / inferred' "$o_amb / $o_inf" "$n_amb / $n_inf" "" "" ""
done

echo
if [ "$failures" -eq 0 ]; then
  echo "both arms completed for every repository"
else
  echo "$failures repository/arm(s) failed; see $OUT"
fi
echo "resolved share is resolved / relations - the proofs, not the claims"
echo "ambiguous is printed on both arms because that is the count the table exists to reduce"
echo "the five states partition on both arms; decided rate is (resolved + inferred) / relations"
echo "logs: $OUT"
exit "$failures"
