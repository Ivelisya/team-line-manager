#!/bin/bash
# 5 分钟资源测量：独立的 ingest（:25912，不中断）+ 一对 agent；每 30 s 记 RSS，结束时算 CPU%
set -uo pipefail
B=$HOME/team-ops-eval/fleet-edge-build; M=$B/measure; BIN=$B/target/x86_64-unknown-linux-musl/release
XRAY=$HOME/team-ops-eval/relays/bin/xray
DUR=${DUR:-300}
rm -rf $M; mkdir -p $M/ns $M/rs
for h in m-node m-relay; do $BIN/ingest-testd agent-token issue $h --db $M/ingest.db > $M/$h.tok 2>/dev/null; chmod 600 $M/$h.tok; done
printf 'endpoint = "http://127.0.0.1:25912"\nhost_id = "m-node"\ntoken_file = "%s"\nstate_dir = "%s"\n' $M/m-node.tok $M/ns > $M/n.toml
printf 'endpoint = "http://127.0.0.1:25912"\nhost_id = "m-relay"\ntoken_file = "%s"\nstate_dir = "%s"\n[xray]\napi = "127.0.0.1:25911"\nbin = "%s"\nproc_match = "srv.json"\nseed_file = ""\n' $M/m-relay.tok $M/rs $XRAY > $M/r.toml
$BIN/ingest-testd serve --db $M/ingest.db --listen 127.0.0.1:25912 > $M/ingest.log 2>&1 & IP=$!
sleep 1
$BIN/nodeagent --config $M/n.toml > $M/n.log 2>&1 & NP=$!
$BIN/relayd --config $M/r.toml > $M/r.log 2>&1 & RP=$!
TCK=$(getconf CLK_TCK)
cpu() { awk '{print $14+$15}' /proc/$1/stat; }
sleep 5
N0=$(cpu $NP); R0=$(cpu $RP); T0=$(date +%s.%N)
maxn=0; maxr=0
for i in $(seq $((DUR/30))); do
  sleep 30
  n=$(awk '/VmRSS/{print $2}' /proc/$NP/status); r=$(awk '/VmRSS/{print $2}' /proc/$RP/status)
  [ $n -gt $maxn ] && maxn=$n; [ $r -gt $maxr ] && maxr=$r
  echo "t=$((i*30))s rss_kb nodeagent=$n relayd=$r"
done
N1=$(cpu $NP); R1=$(cpu $RP); T1=$(date +%s.%N)
python3 - <<P
el=$T1-$T0; t=$TCK
print(f"elapsed={el:.0f}s nodeagent cpu={($N1-$N0)/t/el*100:.4f}% of a core ({($N1-$N0)} ticks), max_rss={$maxn/1024:.1f} MiB")
print(f"elapsed={el:.0f}s relayd    cpu={($R1-$R0)/t/el*100:.4f}% of a core ({($R1-$R0)} ticks), max_rss={$maxr/1024:.1f} MiB")
P
grep -h VmHWM /proc/$NP/status /proc/$RP/status
echo "batches accepted: $(sqlite3 $M/ingest.db 'select host_id, batches, samples from host_latest' 2>/dev/null | tr '\n' ' ')"
kill -TERM $NP $RP; sleep 1; kill -TERM $IP
grep -c push_ok $M/n.log $M/r.log
