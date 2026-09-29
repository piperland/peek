#!/usr/bin/env bash
# Run the engine suite three times and report every failure, by name.
#
# Two failures appeared in a full-target run and did not appear in a `--lib` run, and a
# subsequent full-target run was clean. An intermittent failure is a defect, not noise, so
# it gets measured rather than re-run until green.
set -uo pipefail
cd "$(dirname "$0")/.."
for i in 1 2 3; do
  echo "=== run $i ==="
  cargo test -p peek-core --no-fail-fast 2>&1 | grep -E '^test .*FAILED|^test result'
done
echo FLAKE-DONE
