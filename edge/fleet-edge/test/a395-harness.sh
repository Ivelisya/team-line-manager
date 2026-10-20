#!/bin/bash
# a395 端到端验收夹具（只在 ~/team-ops-eval/fleet-edge-build/run 下运行，只监听 127.0.0.1:259xx）
#   a395-harness.sh setup | start-ingest | stop-ingest | start-xray | stop-xray | start-agents | stop-agents | traffic-on | traffic-off | stop-all | status
set -uo pipefail
B=$HOME/team-ops-eval/fleet-edge-build
R=$B/run
BIN=$B/target/x86_64-unknown-linux-musl/release
FX=$B/src/test/fixture
XRAY=$HOME/team-ops-eval/relays/bin/xray
mkdir -p $R/pids $R/logs
pidof_() { [ -f $R/pids/$1 ] && kill -0 "$(cat $R/pids/$1)" 2>/dev/null && cat $R/pids/$1; }
start_() { local n=$1; shift; pidof_ $n >/dev/null && { echo "$n already running"; return; }; setsid "$@" >>$R/logs/$n.log 2>&1 < /dev/null & echo $! > $R/pids/$n; echo "started $n pid $!"; }
stop_() { local p; p=$(pidof_ $1) && { kill -TERM $p; for i in $(seq 50); do kill -0 $p 2>/dev/null || break; sleep 0.1; done; echo "stopped $1"; }; rm -f $R/pids/$1; }
case "$1" in
setup)
  rm -rf $R; mkdir -p $R/pids $R/logs $R/node-state $R/relay-state
  head -c 20000000 /dev/urandom > $R/blob
  for h in a395-node a395-relay; do
    $BIN/ingest-testd agent-token issue $h --db $R/ingest.db > $R/$h.token 2>/dev/null; chmod 600 $R/$h.token
  done
  cat > $R/node.toml <<T
endpoint = "http://127.0.0.1:25910"
host_id = "a395-node"
token_file = "$R/a395-node.token"
state_dir = "$R/node-state"
T
  cat > $R/relay.toml <<T
endpoint = "http://127.0.0.1:25910"
host_id = "a395-relay"
token_file = "$R/a395-relay.token"
state_dir = "$R/relay-state"
[xray]
api = "127.0.0.1:25911"
bin = "$XRAY"
proc_match = "srv.json"
seed_file = "$R/seed.json"
T
  PUB=$(cat $HOME/team-ops-eval/fleet-probe-build/lab/pub.txt)
  echo "{\"assets\":[{\"outbound_tag\":\"lab-aes\",\"uuid\":\"lab-node-aes\",\"name\":\"lab aes\",\"ip\":\"$PUB\"},{\"outbound_tag\":\"lab-wrongpw\",\"uuid\":\"lab-node-wrongpw\",\"name\":\"lab wrong pw\",\"ip\":\"$PUB\"},{\"outbound_tag\":\"lab-closed\",\"uuid\":\"lab-node-closed\",\"name\":\"lab closed\",\"ip\":\"$PUB\"}]}" > $R/ids.json
  cat >> $R/relay.toml <<T
config = "$FX/srv.json"
[probe]
identities = "$R/ids.json"
T
  # 种子：时刻早于 xray 启动 → 第一次读数全部计入
  echo "{\"ts\": $(date +%s), \"source\": \"test\", \"entries\": [{\"kind\":\"inbound\",\"tag\":\"inbound-25921\",\"up\":1000,\"down\":2000},{\"kind\":\"user\",\"tag\":\"client-1\",\"up\":10,\"down\":20}]}" > $R/seed.json
  sleep 1.2; echo setup done ;;
start-ingest) start_ ingest $BIN/ingest-testd serve --db $R/ingest.db --listen 127.0.0.1:25910 ;;
stop-ingest) stop_ ingest ;;
kill-ingest) p=$(pidof_ ingest) && kill -KILL $p && rm -f $R/pids/ingest && echo "killed ingest" ;;
start-xray) start_ ss $HOME/team-ops-eval/fleet-probe-build/tools/bin/ssserver -s 127.0.0.1:25951 -m aes-256-gcm -k fleet-edge-lab-only-1 -U; start_ xsrv $XRAY run -c $FX/srv.json; start_ xcli $XRAY run -c $FX/cli.json; start_ http python3 -m http.server 25941 --bind 127.0.0.1 --directory $R ;;
stop-xray) stop_ ss; stop_ xsrv; stop_ xcli; stop_ http ;;
restart-xsrv) stop_ xsrv; sleep 1; start_ xsrv $XRAY run -c $FX/srv.json ;;
start-agents) start_ nodeagent $BIN/nodeagent --config $R/node.toml; start_ relayd $BIN/relayd --config $R/relay.toml ;;
stop-agents) stop_ nodeagent; stop_ relayd ;;
traffic-on) start_ traffic bash -c "while true; do curl -s -o /dev/null --socks5-hostname 127.0.0.1:25931 http://127.0.0.1:25941/blob; curl -s -o /dev/null -r 0-999999 --socks5-hostname 127.0.0.1:25922 http://127.0.0.1:25941/blob; sleep 4; done" ;;
traffic-off) stop_ traffic ;;
stop-all) for n in traffic nodeagent relayd ingest ss xsrv xcli http; do stop_ $n; done ;;
status) for n in ingest nodeagent relayd xsrv xcli http traffic; do printf "%-10s %s\n" $n "$(pidof_ $n || echo -)"; done ;;
esac
