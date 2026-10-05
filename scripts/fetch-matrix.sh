#!/usr/bin/env bash
# Print the generated matrix artefacts as base64, one file per marker.
#
# The gate runs in the Linux sandbox and `cargo` cannot run on the Windows host, so the two
# published files are written *there*. They have to come back byte-for-byte, and raw text through
# the CLI transport can be reflowed or stripped - base64 cannot be.
set -euo pipefail
cd "$(dirname "$0")/.."
for f in LANGUAGE_MATRIX.md docs/language-matrix.json; do
  echo "###FILE:$f"
  base64 -w0 "$f"
  echo
done
echo B64-DONE