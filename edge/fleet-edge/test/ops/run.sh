#!/bin/bash
# a395 管理模式验收：副本 Xray（systemd --user 单元 fe-xray）+ SS 上游 + 假 fleetd + relayd（systemd-run --user，带与生产单元相同的 seccomp 过滤）
set -uo pipefail
H=$HOME/team-ops-eval; B=$H/fleet-edge-build; OUT=$B/ops; SRC=$B/src/test/ops
XB=$H/fleet-cutover/dist/xray-26.7.28-64d46afb; GEO=$H/fleet-cutover/dist/geo
SC="systemctl --user"
cleanup() { $SC stop fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; $SC reset-failed fe-relayd fe-mock fe-xray fe-ss >/dev/null 2>&1; }
cleanup
rm -rf $OUT; mkdir -p $OUT/mock $OUT/state
python3 $SRC/build.py
mkdir -p $HOME/.config/systemd/user
cat > $HOME/.config/systemd/user/fe-xray.service <<U
[Unit]
Description=fleet-edge ops test replica xray
[Service]
Environment=XRAY_LOCATION_ASSET=$GEO
ExecStart=$XB run -c $OUT/etc/config.json
Restart=always
RestartSec=1
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
sample_interval_s = 2
batch_samples = 2
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
xray_bin = "$XB"
asset_dir = "$GEO"
systemctl = ["systemctl", "--user"]
unit = "fe-xray.service"
poll_s = 2
T
sleep 2
# 与生产单元相同的系统调用过滤（含禁 kill/tgkill），验证 xray CLI 子进程在过滤下能正常工作
systemd-run --user --unit fe-relayd --collect \
  -p NoNewPrivileges=yes -p SystemCallArchitectures=native -p "SystemCallFilter=@system-service" \
  -p "SystemCallFilter=~@privileged @debug @mount @module @reboot @swap @clock @raw-io @cpu-emulation @obsolete" \
  -p "SystemCallFilter=~kill tkill tgkill rt_sigqueueinfo rt_tgsigqueueinfo pidfd_send_signal process_vm_writev" \
  -p SystemCallErrorNumber=EPERM -p MemoryMax=192M -p CPUQuota=50% \
  $B/target/x86_64-unknown-linux-musl/release/relayd --config $OUT/relayd.toml >/dev/null
sleep 3
python3 $SRC/manage_e2e.py; rc=$?
journalctl --user -u fe-relayd --no-pager -o cat > $OUT/relayd.log 2>&1
$SC show fe-relayd -p MemoryPeak -p CPUUsageNSec -p NRestarts 2>/dev/null
cleanup
exit $rc
