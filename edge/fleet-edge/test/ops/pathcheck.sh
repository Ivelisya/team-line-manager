#!/bin/bash
# a395：原生全链路检查（check=path）验收。副本同 run.sh；relayd 带生产同款 seccomp（禁 kill），检查客户端进程由 PDEATHSIG 回收。
set -uo pipefail
H=$HOME/team-ops-eval; B=$H/fleet-edge-build; OUT=$B/ops; SRC=$B/src/test/ops
XB=$H/fleet-cutover/dist/xray-26.7.28-64d46afb; GEO=$H/fleet-cutover/dist/geo
SC="systemctl --user"
cleanup() { $SC stop fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; $SC reset-failed fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; }
cleanup
START=$(date '+%Y-%m-%d %H:%M:%S')
rm -rf $OUT; mkdir -p $OUT/mock $OUT/state
python3 $SRC/build.py >/dev/null
# 身份映射：出口 tag → 期望出口（副本里所有 SS 上游都从 a395 出网）
python3 - <<PY
import json
c=json.load(open("$OUT/etc/config.json")); e={x["port"]:x for x in json.load(open("$OUT/entries.json"))["entries"]}
assets=[]
for r in c["routing"]["rules"]:
    t=r.get("inboundTag",[None])[0]
    if t and t.startswith("inbound-") and "ruleTag" not in r:
        p=int(t.split("-")[1]); assets.append({"outbound_tag":r["outboundTag"],"uuid":"node-%d"%p,"name":t,"ip":e[p]["expected_ip"]})
json.dump({"assets":assets},open("$OUT/ids.json","w"))
PY
cat > $HOME/.config/systemd/user/fe-xray.service <<U
[Unit]
Description=fleet-edge ops test replica xray
[Service]
Environment=XRAY_LOCATION_ASSET=$GEO
ExecStart=$XB run -c $OUT/etc/config.json
U
$SC daemon-reload
systemd-run --user --unit fe-ss --collect $H/fleet-cutover/dist/linux/ssserver -c $OUT/ss-upstreams.json >/dev/null
$SC start fe-xray
systemd-run --user --unit fe-mock --collect python3 $SRC/mock_fleetd.py $OUT/mock 25920 >/dev/null
echo "fe1_test_token_ops_replica_000000000000" > $OUT/token; chmod 600 $OUT/token
cat > $OUT/relayd.toml <<T
endpoint = "http://127.0.0.1:25920"
host_id = "ops-replica"
token_file = "$OUT/token"
state_dir = "$OUT/state"
[xray]
config = "$OUT/etc/config.json"
bin = "xray-26.7.28-64d46afb"
proc_match = "$OUT/etc/config.json"
seed_file = ""
[probe]
identities = "$OUT/ids.json"
udp = false
ipv6 = false
path_interval_s = 30
[manage]
enabled = false
[relay]
asset_uuid = "relay-self-test"
public_ip = "$(cat $H/fleet-probe-build/lab/pub.txt)"
peers = ["peer-up@127.0.0.1:36001", "peer-down@127.0.0.1:36999"]
peer_interval_s = 10
T
sleep 2
systemd-run --user --unit fe-relayd --collect \
  -p NoNewPrivileges=yes -p SystemCallArchitectures=native -p "SystemCallFilter=@system-service" \
  -p "SystemCallFilter=~@privileged @debug @mount @module @reboot @swap @clock @raw-io @cpu-emulation @obsolete" \
  -p "SystemCallFilter=~kill tkill tgkill rt_sigqueueinfo rt_tgsigqueueinfo pidfd_send_signal process_vm_writev" \
  -p SystemCallErrorNumber=EPERM -p MemoryMax=128M \
  $B/target/x86_64-unknown-linux-musl/release/relayd --config $OUT/relayd.toml >/dev/null
python3 $SRC/pathcheck_verify.py; rc=$?
journalctl --user -u fe-relayd --since "$START" --no-pager -o cat > $OUT/relayd.log 2>&1
python3 $SRC/relayself_verify.py || rc=1
$SC show fe-relayd -p MemoryPeak -p CPUUsageNSec -p NRestarts 2>/dev/null
cleanup
exit $rc
