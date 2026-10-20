#!/bin/bash
# retire-komari-agent.sh <host_id> <ip> <jump|direct> [--key FILE] [--undo] [--dry-run] [--force]
#   停用并禁用 komari-agent（不删文件；原状态记在 /root/fleet-retired-komari-agent.json，--undo 按记录恢复）。
#   前置检查：本机的 fleet-nodeagent（线路机上是 fleet-relayd）在运行、90 s 内有成功上报（--force 跳过）。
#   节点：retire-komari-agent.sh 团队1-24 75.37.237.106 central       （密钥按 IP 查 hosts-keys.txt，同 rollout.sh）
#   线路机：retire-komari-agent.sh line3 38.58.57.69 direct
# 由 main 在 fleetd 影子核对通过后执行。
set -euo pipefail
INVENTORY=${INVENTORY:-/private/tmp/claude-501/-Users-kismet-Desktop-leap/342586b4-73dc-433e-a4bc-cb91efbdbfe7/scratchpad/hosts-keys.txt}
MODE=retire DRY=0 FORCE=0 KEY= POS=()
while [ $# -gt 0 ]; do
  case "$1" in --undo) MODE=undo ;; --dry-run) DRY=1 ;; --force) FORCE=1 ;; --key) KEY=$2; shift ;; -*) echo "未知参数 $1" >&2; exit 2 ;; *) POS+=("$1") ;; esac
  shift
done
[ ${#POS[@]} -eq 3 ] || { echo "用法：retire-komari-agent.sh <host_id> <ip> <jump|direct> [--key F] [--undo] [--dry-run] [--force]" >&2; exit 2; }
HOST=${POS[0]} IP=${POS[1]} JUMP=${POS[2]}
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=15)
if [ "$JUMP" = direct ]; then SSH+=("root@$IP"); else
  [ -z "$KEY" ] && [ -f "$INVENTORY" ] && KEY=$(awk -v ip="$IP" '$2==ip {print $4; exit}' "$INVENTORY")
  KEY=${KEY:-~/.ssh/id_ed25519_38_58_57_22}; KEY=${KEY/#\~/$HOME}
  SSH+=(-o IdentitiesOnly=yes -i "$KEY" -J "$JUMP" "root@$IP")
fi
"${SSH[@]}" "MODE=$MODE DRY=$DRY FORCE=$FORCE bash -s" <<'EOS'
set -euo pipefail
U=komari-agent.service REC=/root/fleet-retired-komari-agent.json
run() { if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi; }
if [ "$MODE" = retire ]; then
  if [ "$FORCE" != 1 ]; then
    S=
    for s in fleet-nodeagent fleet-relayd; do systemctl is-active --quiet $s.service && S=$s; done
    [ -n "$S" ] || { echo "fleet-nodeagent / fleet-relayd 都没在运行，拒绝（--force 跳过）"; exit 3; }
    python3 -c "import json,time,sys; s=json.load(open('/var/lib/$S/status.json')); sys.exit(0 if time.time()-s.get('last_ok',0)<90 else 1)" 2>/dev/null \
      || { echo "$S 最近 90 s 没有成功上报，拒绝（--force 跳过）"; exit 3; }
  fi
  [ -f "$REC" ] && { echo "已经停用过（$REC 存在）"; exit 0; }
  EN=$(systemctl is-enabled $U 2>/dev/null || true); AC=$(systemctl is-active $U 2>/dev/null || true)
  run systemctl disable --now $U || true
  [ "$DRY" = 1 ] || printf '{"time":%s,"enabled":"%s","active":"%s"}\n' "$(date +%s)" "$EN" "$AC" > "$REC"
  echo "komari-agent 已停用（原 enabled=$EN active=$AC）"
else
  [ -f "$REC" ] || { echo "没有停用记录，无需恢复"; exit 0; }
  EN=$(python3 -c "import json;print(json.load(open('$REC'))['enabled'])"); AC=$(python3 -c "import json;print(json.load(open('$REC'))['active'])")
  [ "$EN" = enabled ] && run systemctl enable $U
  [ "$AC" = active ] && run systemctl start $U
  run rm -f "$REC"
  echo "komari-agent 已恢复（enabled=$EN active=$AC）"
fi
EOS
