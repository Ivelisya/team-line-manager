#!/bin/bash
# 端到端：上报 → 中控停机 150 s（期间重启两个 agent）→ 恢复补传 → Xray 重启 → 核对无丢失、无重复、累计值精确
set -uo pipefail
H=$HOME/team-ops-eval/fleet-edge-build/src/test/a395-harness.sh
R=$HOME/team-ops-eval/fleet-edge-build/run
XRAY=$HOME/team-ops-eval/relays/bin/xray
log() { echo "[$(date +%T)] $*"; }
raw() { $XRAY api statsquery --server=127.0.0.1:25911 2>/dev/null | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps({x['name']:int(x.get('value',0)) for x in d.get('stat',[])}))"; }
log "start agents"; $H start-agents
sleep 90
log "OUTAGE: kill -9 ingest"; $H kill-ingest
sleep 60
log "restart agents during outage (SIGTERM → start)"; $H stop-agents; sleep 2; $H start-agents
sleep 90
log "ingest back"; $H start-ingest
sleep 60
log "stop traffic, settle"; $H traffic-off; sleep 25
raw > $R/raw-before-restart.json; log "raw before xray restart: $(cat $R/raw-before-restart.json)"
log "restart xray server"; $H restart-xsrv; sleep 3
$H traffic-on; sleep 60; $H traffic-off; sleep 25
raw > $R/raw-after-restart.json; log "raw after: $(cat $R/raw-after-restart.json)"
sleep 40
log "done"
