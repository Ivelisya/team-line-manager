#!/bin/bash
# measure_all.sh — 在 a395 上（宿主侧）依次测：旧后端基线（fleetd 停）→ fleetd 空闲节奏 → 有 SSE 客户端的活跃节奏，
# 然后跑 SSE / 鉴权 / 故障注入场景。结果写 ~/team-ops-eval/fleet-stage/results/*.json。
# 用法：nohup bash measure_all.sh [BASE_S] [PHASE_S] > results/run.log 2>&1 &
set -u
B=~/team-ops-eval/fleet-stage
PY=~/team-ops-eval/teamops/runtime/python/bin/python3.12
BASE_S=${1:-180}
PHASE_S=${2:-300}
mkdir -p $B/results
cd $B
st() { curl -s -m 5 http://127.0.0.1:18397/__fleet/status | $PY -c 'import json,sys; d=json.load(sys.stdin)["counts"]; print(d["legacy"], d["rpc2"], d["me"])'; }
fl() { curl -s -m 5 -X POST -H 'Content-Type: application/json' --data "{\"action\":\"$1\"}" http://127.0.0.1:18397/__fleet/fleetd; echo; }
echo "== baseline (fleetd stopped) $(date)"
fl stop; sleep 15
c0=$(st); $PY measure.py $BASE_S > results/m_baseline.json; c1=$(st)
echo "baseline legacy/rpc2/me: $c0 -> $c1"
echo "== idle cadence $(date)"
fl start; sleep 30
c0=$(st); $PY measure.py $PHASE_S > results/m_idle.json; c1=$(st)
echo "idle legacy/rpc2/me: $c0 -> $c1"
echo "== active cadence (1 SSE client) $(date)"
curl -sN -m $((PHASE_S + 60)) -H "Cookie: session_token=$(cat session.token)" http://127.0.0.1:18396/v2/api/stream > results/sse_active.log &
SSE=$!
sleep 15
c0=$(st); $PY measure.py $PHASE_S > results/m_active.json; c1=$(st)
echo "active legacy/rpc2/me: $c0 -> $c1"
kill $SSE 2>/dev/null
curl -s -m 5 http://127.0.0.1:18396/v2/api/v1/system > results/system_after_active.json
echo "== verify $(date)"
$PY verify_stage.py equal > results/v_equal_keepalive.json 2>&1
$PY verify_stage.py sse > results/v_sse.json 2>&1
$PY verify_stage.py auth > results/v_auth.json 2>&1
$PY verify_stage.py scenario > results/v_scenario.json 2>&1
curl -s -m 5 http://127.0.0.1:18396/v2/api/v1/system > results/system_end.json
echo "== done $(date)"
