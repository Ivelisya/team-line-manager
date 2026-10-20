#!/bin/bash
# retire-legacy-relay.sh <relay> [--undo] [--dry-run] [--force]
#   在线路机上停用被 relayd 取代的旧组件（不删任何文件；原状态记在 /root/fleet-retired-legacy.json，--undo 按记录恢复）：
#   - komari-relay-probe.service / komari-relay-xray.service（隔离 xray）/ komari-relay-source-sync.timer  → relayd 内嵌探测取代
#   - team-ops-path-probe.timer                                                                      → relayd 内嵌探测取代
#   - /root/.ssh/authorized_keys 里 team-ops-restricted-bridge 这把强制命令密钥（整行加注释前缀 "#fleet-retired# "）
#                                                                                                    → relayd 上报 + 管理模式取代
#   保留：team-ops-logrotate.timer（它负责轮转 Xray 访问日志，relayd 的 tail 依赖正常轮转）、komari-agent（另用 retire-komari-agent.sh）。
#   前置检查：fleet-relayd 在运行、90 s 内有成功上报、探测在跑（--force 跳过）。
# <relay> 是 ~/.ssh/config 里的别名：central / line1 / line3。由 main 在 fleetd 影子核对通过后执行。
set -euo pipefail
R=${1:?用法：retire-legacy-relay.sh <relay> [--undo] [--dry-run] [--force]}; shift
MODE=retire DRY=0 FORCE=0
for a in "$@"; do case "$a" in --undo) MODE=undo ;; --dry-run) DRY=1 ;; --force) FORCE=1 ;; *) echo "未知参数 $a" >&2; exit 2 ;; esac; done
ssh -o BatchMode=yes -o ConnectTimeout=15 "$R" "MODE=$MODE DRY=$DRY FORCE=$FORCE bash -s" <<'EOS'
set -euo pipefail
UNITS="komari-relay-probe.service komari-relay-xray.service komari-relay-source-sync.timer team-ops-path-probe.timer"
REC=/root/fleet-retired-legacy.json
AK=/root/.ssh/authorized_keys
MARK="#fleet-retired# "
run() { if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi; }
if [ "$MODE" = retire ]; then
  if [ "$FORCE" != 1 ]; then
    systemctl is-active --quiet fleet-relayd.service || { echo "fleet-relayd 没在运行，拒绝（--force 跳过）"; exit 3; }
    python3 - <<'PY' || { echo "fleet-relayd 最近 90 s 没有成功上报，拒绝（--force 跳过）"; exit 3; }
import json, time, sys
s = json.load(open("/var/lib/fleet-relayd/status.json"))
sys.exit(0 if time.time() - s.get("last_ok", 0) < 90 else 1)
PY
    [ -s /var/lib/fleet-relayd/probe.db ] || { echo "relayd 探测数据库不存在（探测没在跑？），拒绝（--force 跳过）"; exit 3; }
  fi
  if [ -f "$REC" ]; then echo "已经停用过（$REC 存在）；要重来先 --undo"; exit 0; fi
  python3 - "$REC" $UNITS <<'PY' > /tmp/fleet-retire.rec
import json, subprocess, sys, time
out = {"time": time.time(), "units": {}}
for u in sys.argv[2:]:
    q = lambda *a: subprocess.run(["systemctl", *a, u], capture_output=True, text=True).stdout.strip()
    out["units"][u] = {"enabled": q("is-enabled"), "active": q("is-active")}
print(json.dumps(out))
PY
  cat /tmp/fleet-retire.rec
  for u in $UNITS; do
    if systemctl list-unit-files --no-legend "$u" | grep -q .; then run systemctl disable --now "$u" || true; fi
  done
  # 定时器触发的 oneshot 也停掉（如果正在跑）
  run systemctl stop komari-relay-source-sync.service team-ops-path-probe.service 2>/dev/null || true
  if [ -f "$AK" ] && grep -q 'team-ops-restricted-bridge' "$AK"; then
    run cp -p "$AK" "$AK.fleet-retire-$(date +%Y%m%d%H%M%S)"
    run sed -i "/team-ops-restricted-bridge/{/^$MARK/!s/^/$MARK/}" "$AK"
  fi
  if [ "$DRY" != 1 ]; then install -m 0600 /tmp/fleet-retire.rec "$REC"; fi
  rm -f /tmp/fleet-retire.rec
  echo "已停用：$UNITS；bridge 密钥行已注释"
else
  [ -f "$REC" ] || { echo "没有停用记录 $REC，无需恢复"; exit 0; }
  python3 - "$REC" <<'PY' | while read -r u en ac; do
import json, sys
for u, s in json.load(open(sys.argv[1]))["units"].items():
    print(u, s["enabled"], s["active"])
PY
    [ "$en" = enabled ] && run systemctl enable "$u"
    [ "$ac" = active ] && run systemctl start "$u"
    echo "恢复 $u（enabled=$en active=$ac）"
  done
  [ -f "$AK" ] && run sed -i "s/^$MARK//" "$AK"
  run rm -f "$REC"
  echo "已恢复"
fi
EOS
