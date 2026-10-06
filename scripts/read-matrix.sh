#!/usr/bin/env bash
# Print Rust's gate figures as numerator/denominator per dimension, from the published matrix.
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'EOF'
import json
d = json.load(open('docs/language-matrix.json'))
want = ('symbol_precision','symbol_recall','definitions','members','resolution_correctness',
        'references','calls','imports','incremental','query','context')
for r in d['rows']:
    if r['language'] == 'rust':
        for k, v in r.get('dimensions', {}).items():
            if k in want:
                print(f"{k} = {v['numerator']}/{v['denominator']}")
EOF
echo JSON-DONE