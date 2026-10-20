#!/bin/bash
# run.sh — sync the v2 static build + staging server to a395 and (re)start it.
#
#   deploy/v2/staging/run.sh            sync + restart (default)
#   deploy/v2/staging/run.sh status     is it running, which as_of, which clock
#   deploy/v2/staging/run.sh stop       stop it
#   deploy/v2/staging/run.sh logs       last 60 log lines
#
# Static files: deploy/v2/dist if it exists, otherwise a preview (prototype/shared + live adapter + a
# smoke page).  Fixtures (live/fixtures, real IPs) are never copied.  Everything lands under
# ~/team-ops-eval/v2-stage on a395; the server binds 127.0.0.1:$PORT only.
# Open it from this Mac:  ssh -i ~/.ssh/a395_kismet -N -L 18395:127.0.0.1:18395 kismet@10.172.17.213
#                         then http://127.0.0.1:18395/
# Env: A395 (kismet@10.172.17.213)  KEY (~/.ssh/a395_kismet)  PORT (18395)  CLOCK (keepalive|shifted|pinned)
#      AUTH_TOKEN (optional: require cookie stage_session=<token> on /api/team-ops/*)
#      STAGE_WRITES=1 (optional: accept the class (a) team-ops DB writes on the fresh DB copy; default read-only)
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
PORT=${PORT:-18395}
CLOCK=${CLOCK:-keepalive}
HERE=$(cd "$(dirname "$0")" && pwd)
V2=$(dirname "$HERE")
ROOT=$(cd "$V2/../.." && pwd)
R='~/team-ops-eval/v2-stage'
PY='~/team-ops-eval/teamops/runtime/python/bin/python3.12'
SSH=(ssh -i "$KEY" -o ConnectTimeout=10 "$A395")
export COPYFILE_DISABLE=1   # macOS tar: no ._ files / xattr headers in the archives

# stop only the process recorded in stage.pid, and only if it really is our python stage server
remote_stop='cd '"$R"' && if [ -f stage.pid ]; then P=$(cat stage.pid); if ps -o args= -p "$P" 2>/dev/null | grep -q "^[^ ]*python3[.0-9]* staging/stage_server.py serve"; then kill "$P"; for i in $(seq 1 30); do kill -0 "$P" 2>/dev/null || break; sleep 0.5; done; fi; rm -f stage.pid; fi'

case "${1:-start}" in
stop) "${SSH[@]}" "$remote_stop"; echo "stopped"; exit 0 ;;
logs) "${SSH[@]}" "tail -n 60 $R/stage.log"; exit 0 ;;
status) "${SSH[@]}" "cd $R; [ -f stage.pid ] && ps -o pid=,etime=,args= -p \$(cat stage.pid) || echo 'not running'; curl -s -m 5 http://127.0.0.1:$PORT/__stage/status; echo"; exit 0 ;;
start) ;;
*) echo "usage: $0 [start|stop|status|logs]"; exit 2 ;;
esac

# 1) assemble the static tree locally
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
if [ -d "$V2/dist" ]; then
  echo "static: deploy/v2/dist"
  cp -R "$V2/dist/." "$STAGE/"
else
  echo "static: preview (no deploy/v2/dist yet)"
  mkdir -p "$STAGE/shared" "$STAGE/live" "$STAGE/data"
  (cd "$ROOT/prototype/shared" && tar cf - --exclude='*.test.mjs' --exclude='smoke.html' .) | (cd "$STAGE/shared" && tar xf -)
  cp "$V2/live/adapter.js" "$V2/live/synth.js" "$STAGE/live/"
  cp "$ROOT/prototype/data/plan-catalog.json" "$STAGE/data/"
  cp "$HERE/preview/index.html" "$HERE/preview/preview.js" "$STAGE/"
fi
if find "$STAGE" -path '*fixtures*' | grep -q .; then echo "refusing: fixtures in static tree" >&2; exit 1; fi

# 2) sync code + static (no rsync on a395: tar over ssh, then swap)
"${SSH[@]}" "mkdir -p $R/staging && rm -rf $R/static.new && mkdir -p $R/static.new"
tar --no-xattrs -C "$HERE" -czf - stage_server.py komari_mirror.py | "${SSH[@]}" "tar -xzf - -C $R/staging"
tar --no-xattrs -C "$STAGE" -czf - . | "${SSH[@]}" "tar -xzf - -C $R/static.new && rm -rf $R/static.old && { [ -d $R/static ] && mv $R/static $R/static.old || true; } && mv $R/static.new $R/static && rm -rf $R/static.old"

# 3) restart under nohup with a pidfile (fresh DB copy is made by the server on every start)
AUTH=""
[ -n "${AUTH_TOKEN:-}" ] && AUTH="--auth-token $AUTH_TOKEN"
[ "${STAGE_WRITES:-0}" = 1 ] && AUTH="$AUTH --writes"   # class (a) write routes on the DB copy (LIVE-MODE.md §13)
"${SSH[@]}" "$remote_stop; cd $R || exit 1; nohup $PY staging/stage_server.py serve --static $R/static --port $PORT --clock $CLOCK $AUTH >> stage.log 2>&1 < /dev/null & echo \$! > $R/stage.pid"
for i in $(seq 1 60); do
  if "${SSH[@]}" "curl -s -m 2 -o /dev/null -w '%{http_code}' http://127.0.0.1:$PORT/__stage/status" 2>/dev/null | grep -q 200; then
    "${SSH[@]}" "curl -s http://127.0.0.1:$PORT/__stage/status"; echo
    echo "staging up on a395 127.0.0.1:$PORT  →  ssh -i $KEY -N -L $PORT:127.0.0.1:$PORT $A395  then  http://127.0.0.1:$PORT/"
    exit 0
  fi
  sleep 1
done
echo "staging did not come up; last log lines:" >&2
"${SSH[@]}" "tail -n 30 $R/stage.log" >&2
exit 1
