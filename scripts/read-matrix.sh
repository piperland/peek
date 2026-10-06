#!/usr/bin/env bash
# Print Rust's gate figures as numerator/denominator per dimension, from the published matrix.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'EOF'
import json, re
raw = open('docs/language-matrix.json').read()
want = ('symbol_precision','symbol_recall','definitions','members','resolution_correctness',
        'references','calls','imports','incremental','query','context')
for k in want:
    m = re.search(r'"%s"\s*:\s*\{\s*"numerator"\s*:\s*(\d+)\s*,\s*"denominator"\s*:\s*(\d+)' % k, raw)
    if m:
        print(f"{k} = {m.group(1)}/{m.group(2)}")
    else:
        m2 = re.search(r'"%s"[^}]{0,200}' % k, raw)
        print(f"{k} = <shape: {(m2.group(0)[:80] if m2 else 'absent')}>")
EOF
echo JSON-DONE