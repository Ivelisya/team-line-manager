#!/bin/bash
# crosscheck_build.sh — run prototype/data/tools/{extract,build}.py on a395 against a FRESH copy of the same
# databases the staging server uses, WITHOUT anonymisation (strings untouched; numbers normalised exactly
# as build.py's anon() does), so deploy/v2/tests/crosscheck.mjs can compare the live adapter's output with
# what the snapshot builder computes from the DB.  Output stays under ~/team-ops-eval/v2-stage/crosscheck.
# usage: bash crosscheck_build.sh   (expects extract.py and build.py next to it in ~/team-ops-eval/v2-stage/crosscheck/tools)
set -euo pipefail
S=~/team-ops-eval/v2-stage
C=$S/crosscheck
PY=~/team-ops-eval/teamops/runtime/python/bin/python3.12
umask 077
rm -rf "$S/db-crosscheck" "$C/out"; mkdir -p "$S/db-crosscheck" "$C/out/series"
for f in operations.db komari.db metrics.db; do cp --reflink=auto ~/team-ops-eval/data/$f "$S/db-crosscheck/$f"; done
sed -e "s#~/team-ops-eval/teamops/db/operations-backup-20260923.db#$S/db-crosscheck/operations.db#" \
    -e "s#~/team-ops-eval/data/komari.db#$S/db-crosscheck/komari.db#" \
    -e "s#~/team-ops-eval/data/metrics.db#$S/db-crosscheck/metrics.db#" "$C/tools/extract.py" > "$C/extract_cc.py"
# build.py with string anonymisation switched off (anon_str → identity); numeric rounding kept
$PY - "$C/tools/build.py" "$C/build_cc.py" <<'PY'
import sys
src = open(sys.argv[1]).read()
src = src.replace("def anon_str(s):\n", "def anon_str(s):\n    return s\n\n\ndef _anon_str_disabled(s):\n", 1)
open(sys.argv[2], "w").write(src)
PY
nice -n 10 $PY "$C/extract_cc.py" > "$C/raw.json"
$PY "$C/build_cc.py" "$C/raw.json" "$C/out" | tail -c 600
rm -f "$C/raw.json"
ls -la "$C/out"
