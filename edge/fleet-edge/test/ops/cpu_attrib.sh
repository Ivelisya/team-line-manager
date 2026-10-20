#!/bin/bash
# a395：relayd CPU 归因（副本 + 假 fleetd），每个变体跑 DUR 秒，记进程 user+sys 与子进程
set -uo pipefail
H=$HOME/team-ops-eval; B=$H/fleet-edge-build; OUT=$B/ops; SRC=$B/src/test/ops
XB=$H/fleet-cutover/dist/xray-26.7.28-64d46afb; GEO=$H/fleet-cutover/dist/geo; SC="systemctl --user"; DUR=${DUR:-60}
cleanup() { $SC stop fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; $SC reset-failed fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; }
cleanup; rm -rf $OUT; mkdir -p $OUT/mock; python3 $SRC/build.py >/dev/null
python3 - <<PY
import json
c=json.load(open("$OUT/etc/config.json")); e={x["port"]:x for x in json.load(open("$OUT/entries.json"))["entries"]}
a=[{"outbound_tag":r["outboundTag"],"uuid":"node-%s"%r["inboundTag"][0],"name":"x","ip":e[int(r["inboundTag"][0].split("-")[1])]["expected_ip"]} for r in c["routing"]["rules"] if r.get("inboundTag",[""])[0].startswith("inbound-") and "ruleTag" not in r]
json.dump({"assets":a},open("$OUT/ids.json","w"))
PY
cat > $HOME/.config/systemd/user/fe-xray.service <<U
[Service]
Environment=XRAY_LOCATION_ASSET=$GEO
ExecStart=$XB run -c $OUT/etc/config.json
U
$SC daemon-reload; $SC start fe-xray
systemd-run --user --unit fe-ss --collect $H/fleet-cutover/dist/linux/ssserver -c $OUT/ss-upstreams.json >/dev/null
systemd-run --user --unit fe-mock --collect python3 $SRC/mock_fleetd.py $OUT/mock 25920 >/dev/null
echo "fe1_test_token_ops_replica_000000000000" > $OUT/token; chmod 600 $OUT/token
TCK=$(getconf CLK_TCK)
run() { # name interval batch probe path access manage
  rm -rf $OUT/state; mkdir -p $OUT/state
  cat > $OUT/v.toml <<T
endpoint = "http://127.0.0.1:25920"
host_id = "ops-replica"
token_file = "$OUT/token"
state_dir = "$OUT/state"
sample_interval_s = $2
batch_samples = $3
[xray]
config = "$OUT/etc/config.json"
bin = "xray-26.7.28-64d46afb"
proc_match = "$OUT/etc/config.json"
seed_file = ""
[probe]
enabled = $4
path = $5
identities = "$OUT/ids.json"
[access]
enabled = $6
[manage]
enabled = $7
config = "$OUT/etc/config.json"
lock = "$OUT/lock"
T
  $B/target/x86_64-unknown-linux-musl/release/relayd --config $OUT/v.toml 2>/dev/null & P=$!
  sleep 8; a=$(awk '{print $14+$15, $16+$17}' /proc/$P/stat); sleep $DUR; b=$(awk '{print $14+$15, $16+$17}' /proc/$P/stat)
  kill -TERM $P; wait $P 2>/dev/null
  python3 -c "a='$a'.split(); b='$b'.split(); print('%-28s self %.3f%%  children %.3f%%' % ('$1', (int(b[0])-int(a[0]))/$TCK/$DUR*100, (int(b[1])-int(a[1]))/$TCK/$DUR*100))"
}
run "all 5s/2"            5 2 true true true true
run "all 10s/3 (old)"     10 3 true true true true
run "no path"             5 2 true false true true
run "no probe(no path)"   5 2 false false true true
run "no access"           5 2 true true false true
run "no manage"           5 2 true true true false
run "bare (stats+host)"   5 2 false false false false
cleanup
