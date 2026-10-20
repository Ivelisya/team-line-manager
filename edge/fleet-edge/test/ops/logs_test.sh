#!/bin/bash
# a395：xray_logs（generation 0 的只读命令）验收
set -uo pipefail
H=$HOME/team-ops-eval; B=$H/fleet-edge-build; OUT=$B/ops; SRC=$B/src/test/ops
XB=$H/fleet-cutover/dist/xray-26.7.28-64d46afb; GEO=$H/fleet-cutover/dist/geo; SC="systemctl --user"
cleanup() { $SC stop fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; $SC reset-failed fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; }
cleanup; rm -rf $OUT; mkdir -p $OUT/mock $OUT/state; python3 $SRC/build.py >/dev/null
cat > $HOME/.config/systemd/user/fe-xray.service <<U
[Service]
Environment=XRAY_LOCATION_ASSET=$GEO
ExecStart=$XB run -c $OUT/etc/config.json
U
$SC daemon-reload; $SC start fe-xray
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
enabled = false
[manage]
config = "$OUT/etc/config.json"
lock = "$OUT/lock"
unit = "fe-xray.service"
systemctl = ["systemctl", "--user"]
poll_s = 2
T
for i in $(seq 30); do echo "2026/09/24 07:50:$i from 198.51.100.$i:1 accepted tcp:h$i.example.com:443 [inbound-36002 -> direct]" >> $OUT/log/xray-access.log; done
systemd-run --user --unit fe-relayd --collect -p "SystemCallFilter=@system-service" -p "SystemCallFilter=~kill tkill tgkill pidfd_send_signal" -p SystemCallErrorNumber=EPERM \
  $B/target/x86_64-unknown-linux-musl/release/relayd --config $OUT/relayd.toml >/dev/null
sleep 3
python3 - <<PY
import json, time, os
M="$OUT/mock"
spec={"schema":"fleet.relayspec.v1","relay":"ops-replica","generation":0,"spec_sha256":None,"base_config_sha256":None,"managed":None,
      "commands":[{"id":"cmd-logs-a","kind":"xray_logs","args":{"which":"access","lines":5}},
                  {"id":"cmd-logs-e","kind":"xray_logs","args":{"which":"error","lines":3}},
                  {"id":"cmd-logs-bad","kind":"xray_logs","args":{"which":"nope"}}]}
json.dump(spec,open(M+"/spec.json","w"))
t0=time.time()
while time.time()-t0<30:
    if os.path.exists(M+"/acks.jsonl"):
        a=[json.loads(l) for l in open(M+"/acks.jsonl")]
        cs={c["id"]:c for x in a for c in x.get("commands",[])}
        if len(cs)>=3: break
    time.sleep(0.5)
print("ack generation", a[-1]["generation"], "status", a[-1]["status"], "took %.1fs" % (time.time()-t0))
for k,c in cs.items(): print(k, json.dumps(c, ensure_ascii=False)[:420])
PY
cleanup
