#!/usr/bin/env bash
# Print every golden as base64, one file per marker, so it can be copied out of the sandbox
# without the content passing through a shell that would reformat it.
set -euo pipefail
cd "$(dirname "$0")/../crates/peek-cli/tests/golden"
for f in *.txt; do
  echo "###FILE:$f"
  base64 -w0 "$f"
  echo
done
echo B64-DONE
