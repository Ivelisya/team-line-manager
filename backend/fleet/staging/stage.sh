#!/bin/bash
# stage.sh — 在 a395 上起 fleetd 测试服（隔离命名空间：旧后端副本 + Komari 替身 + fleetd）。
#   fleet/staging/stage.sh [start] [keepalive|pinned]   同步并（重）启动
#   fleet/staging/stage.sh stop | status | logs
#   fleet/staging/stage.sh inject '{"op":"node_fail","node":"t1-31"}'   （op: node_fail / egress_mismatch / relay_down / quota / clear）
#   fleet/staging/stage.sh api /v2/api/v1/alerts        带测试会话 cookie 调 fleetd
# 只绑 a395 的 127.0.0.1:18396（fleetd）与 127.0.0.1:18397（旧后端副本）；deploy/v2/staging 的代码只拷贝、不修改。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
R='~/team-ops-eval/fleet-stage'
PY='~/team-ops-eval/teamops/runtime/python/bin/python3.12'
SSH=(ssh -i "$KEY" -o ConnectTimeout=10 "$A395")
export COPYFILE_DISABLE=1
remote_stop='cd '"$R"' 2>/dev/null && if [ -f stage.pid ]; then P=$(cat stage.pid); if ps -o args= -p "$P" 2>/dev/null | grep -q "^[^ ]*python3[.0-9]* fleet_stage.py serve"; then kill "$P"; for i in $(seq 1 40); do kill -0 "$P" 2>/dev/null || break; sleep 0.5; done; fi; rm -f stage.pid; fi'
case "${1:-start}" in
stop) "${SSH[@]}" "$remote_stop"; echo stopped; exit 0 ;;
logs) "${SSH[@]}" "tail -n 40 $R/stage.log; echo ---; tail -n 40 $R/fleetd.log"; exit 0 ;;
status) "${SSH[@]}" "curl -s -m 5 http://127.0.0.1:18397/__fleet/status | head -c 2000; echo; curl -s -m 5 http://127.0.0.1:18396/v2/api/v1/system | head -c 3000; echo"; exit 0 ;;
inject) "${SSH[@]}" "curl -s -m 5 -X POST -H 'Content-Type: application/json' --data '$2' http://127.0.0.1:18397/__fleet/inject; echo"; exit 0 ;;
api) "${SSH[@]}" "curl -s -m 10 -H \"Cookie: session_token=\$(cat $R/session.token)\" 'http://127.0.0.1:18396$2'; echo"; exit 0 ;;
start) ;;
*) echo "usage: $0 [start [keepalive|pinned]|stop|status|logs|inject JSON|api PATH]"; exit 2 ;;
esac
CLOCK=${2:-keepalive}
"${SSH[@]}" "mkdir -p $R/vendor $R/bin $R/data && chmod 700 $R"
tar --no-xattrs -C "$ROOT/deploy/v2/staging" -czf - stage_server.py komari_mirror.py | "${SSH[@]}" "tar -xzf - -C $R/vendor"
tar --no-xattrs -C "$HERE" -czf - fleet_stage.py measure.py fleetd.stage.toml onboard.stage.toml ssh-noconf | "${SSH[@]}" "tar -xzf - -C $R"
tar --no-xattrs -C "$ROOT/architecture/plan-catalog" -czf - catalog.json | "${SSH[@]}" "tar -xzf - -C $R"
"${SSH[@]}" "cp ~/team-ops-eval/fleet-build/src/target/x86_64-unknown-linux-musl/release/fleetd $R/bin/fleetd.new && mv $R/bin/fleetd.new $R/bin/fleetd && sed \"s#@HOME@#\$HOME#g\" $R/fleetd.stage.toml > $R/fleetd.toml && sed \"s#@HOME@#\$HOME#g\" $R/onboard.stage.toml > $R/onboard.toml && chmod 755 $R/ssh-noconf && mkdir -p $R/onboard-keys $R/dist && chmod 700 $R/onboard-keys && { [ -f $R/onboard-keys/id_ed25519 ] || { ssh-keygen -q -t ed25519 -N '' -f $R/onboard-keys/id_ed25519 && ssh-keygen -q -t ed25519 -N '' -f $R/onboard-keys/relay_ed25519 && touch $R/onboard-keys/relays.known_hosts; }; }"
"${SSH[@]}" "$remote_stop; cd $R || exit 1; rm -rf data/*; setsid nohup $PY fleet_stage.py serve --clock $CLOCK >> stage.log 2>&1 < /dev/null & echo \$! > $R/stage.pid" < /dev/null
for i in $(seq 1 90); do
  if ssh -n -i "$KEY" -o ConnectTimeout=10 "$A395" "curl -s -m 2 -o /dev/null -w '%{http_code}' http://127.0.0.1:18396/v2/api/v1/system" 2>/dev/null | grep -q 200; then
    "${SSH[@]}" "curl -s http://127.0.0.1:18397/__fleet/status | head -c 600; echo"
    # 测试账户（data/ 每次重建）：密码在 a395 的 $R/test-user.txt（0600）
    "${SSH[@]}" "cd $R && [ -f test-user.txt ] && sed -n 's/^password=//p' test-user.txt | bin/fleetd user set-password uitest --config fleetd.toml >/dev/null && echo 'test user uitest ready'"
    echo "fleet-stage up: fleetd 127.0.0.1:18396, legacy copy 127.0.0.1:18397 (clock=$CLOCK)"
    exit 0
  fi
  sleep 1
done
echo "did not come up" >&2; "${SSH[@]}" "tail -n 30 $R/stage.log; tail -n 20 $R/fleetd.log" >&2; exit 1
