#!/usr/bin/env bash
# Run every mutation check and report each one's real exit status.
#
# A check that exits 0 is a check that ran; a check whose output was piped, tail'd or grepped
# reports the *last* command's status, not the check's. Both mistakes have been made here while
# verifying this project's own gates, so the status is captured explicitly and printed as a
# labelled line rather than inferred from whether output looked alarming.
#
# Usage (inside the verification sandbox, from the repository root):
#   bash scripts/run-mutation-checks.sh
set -uo pipefail
cd "$(dirname "$0")/.."

status=0
for check in language-gate-mutation-check.sh binding-measurement-mutation-check.sh; do
  log="/tmp/mutation-${check%.sh}.log"
  if bash "scripts/$check" > "$log" 2>&1; then
    code=0
  else
    code=$?
  fi
  echo "MUTATION_CHECK: $check exit=$code"
  tail -3 "$log" | sed 's/^/    /'
  [ "$code" -ne 0 ] && status=1
done

echo "MUTATION_CHECKS_OVERALL: $status"
exit "$status"
