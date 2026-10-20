#!/bin/bash
# rollout.sh — 在一台主机上安装 / 升级 / 卸载 fleet-edge 代理（从 Mac 执行，幂等）。
#
# 落地节点（nodeagent）：
#   deploy/rollout.sh 团队1-24 75.37.237.106 central
#   deploy/rollout.sh 团队1-01 75.37.237.38 central --key ~/.ssh/id_ed25519_attvps_75_37_237
# 线路机（relayd，观察模式）：jump 写 direct，走 ~/.ssh/config 里的密钥
#   deploy/rollout.sh --relay line3 38.58.57.69 direct
# 卸载（停止、禁用、删除单元/二进制/配置/状态/用户；加 --revoke 同时在中控撤销令牌）：
#   deploy/rollout.sh --remove 团队1-24 75.37.237.106 central [--revoke]
#
# 选项：
#   --key FILE          节点 SSH 私钥（缺省按 IP 查 hosts-keys.txt，再缺省 ~/.ssh/id_ed25519_38_58_57_22）
#   --relay             装 relayd（缺省 nodeagent）
#   --rotate-token      重新签发令牌（缺省：目标机已有令牌就沿用）
#   --token-file F      不向中控申请，直接用本地文件里的令牌（测试用）
#   --xray-config P     relayd 读的 Xray 配置（缺省 /etc/fleet/xray/config.json：入站/用户/api 地址/探测对象）
#   --xray-api A        relayd 的 Xray API（缺省从 --xray-config 的 api 入站推出；也可 unix:/run/xray/api.sock）
#   --xray-bin P        xray 进程 argv[0] 的文件名（缺省 xray）
#   --xray-match S      xray 命令行必须包含的片段（缺省 /etc/fleet/xray/config.json）
#   --no-probe          relayd 不跑内嵌探测
#   --no-path           relayd 不跑原生全链路检查（check=path）
#   --no-manage         relayd 不开管理模式（fleet.relayspec.v1；缺省开启，fleetd 未下发期望状态时什么都不做）
#   --no-access         relayd 不上报访问日志（fleet.access.v1；缺省开启）
#   --xray-group G      relayd 追加的附加组（读 unix socket 用）
#   --seed FILE         上传累计流量种子到 /etc/fleet-edge/xray-seed.json（见 xui-seed.py）
#   --asset-uuid U      线路机自己的资产 uuid（线路机自检 egress 与被互探时的 node）
#   --public-ip IP      线路机公网 IP（自检的期望出口）
#   --master-port P     主节点入口端口（缺省自动：路由到 freedom 的入口）
#   --peer U@IP:PORT    互探对象（其他线路机的主节点入口，可重复）
#   --interval S        采样间隔秒数（缺省 5；relayd 的 Xray 统计同频）
#   --batch N           每批样本数（缺省 2，即每 10 s 上报一次）
#   --endpoint URL      缺省 https://38.58.57.22
#   --no-wait           不等第一批被接收
#   --force             出站 443 预检失败也继续
#
# 环境变量：
#   TOKEN_CMD  在中控签发令牌的命令（%s 替换为 host_id），缺省：
#              ssh central "runuser -u fleetd -- /opt/fleetd/current/fleetd agent-token issue '%s'"
#   REVOKE_CMD 同上，撤销
#   DIST       二进制目录（缺省 fleet-edge/dist/current）
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
DIST=${DIST:-$ROOT/dist/current}
INVENTORY=${INVENTORY:-/private/tmp/claude-501/-Users-kismet-Desktop-leap/342586b4-73dc-433e-a4bc-cb91efbdbfe7/scratchpad/hosts-keys.txt}
TOKEN_CMD=${TOKEN_CMD:-"ssh central \"runuser -u fleetd -- /opt/fleetd/current/fleetd agent-token issue '%s'\""}
REVOKE_CMD=${REVOKE_CMD:-"ssh central \"runuser -u fleetd -- /opt/fleetd/current/fleetd agent-token revoke '%s'\""}

MODE=install KIND=node KEY= ROTATE=0 TOKFILE= WAIT=1 FORCE=0 REVOKE=0 SEED=
ENDPOINT=https://38.58.57.22
INTERVAL=5 BATCHN=2
RUUID= RIP= RMPORT= RPEERS=()
XAPI= XBIN=xray XMATCH=/etc/fleet/xray/config.json XGROUP= XCONFIG=/etc/fleet/xray/config.json PROBE=1 MANAGE=1 ACCESS=1 PATHCHK=1
POS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --remove) MODE=remove ;;
    --revoke) REVOKE=1 ;;
    --relay) KIND=relay ;;
    --key) KEY=$2; shift ;;
    --rotate-token) ROTATE=1 ;;
    --token-file) TOKFILE=$2; shift ;;
    --xray-api) XAPI=$2; shift ;;
    --xray-config) XCONFIG=$2; shift ;;
    --no-probe) PROBE=0 ;;
    --no-manage) MANAGE=0 ;;
    --no-path) PATHCHK=0 ;;
    --no-access) ACCESS=0 ;;
    --xray-bin) XBIN=$2; shift ;;
    --xray-match) XMATCH=$2; shift ;;
    --xray-group) XGROUP=$2; shift ;;
    --seed) SEED=$2; shift ;;
    --endpoint) ENDPOINT=$2; shift ;;
    --interval) INTERVAL=$2; shift ;;
    --asset-uuid) RUUID=$2; shift ;;
    --public-ip) RIP=$2; shift ;;
    --master-port) RMPORT=$2; shift ;;
    --peer) RPEERS+=("$2"); shift ;;
    --batch) BATCHN=$2; shift ;;
    --no-wait) WAIT=0 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    -*) echo "未知选项 $1" >&2; exit 2 ;;
    *) POS+=("$1") ;;
  esac
  shift
done
[ ${#POS[@]} -eq 3 ] || { echo "用法：rollout.sh [选项] <host_id> <ip> <jump|direct>（--help 看说明）" >&2; exit 2; }
HOST_ID=${POS[0]} IP=${POS[1]} JUMP=${POS[2]}

# relayd 以 root 运行（无任何 capability，见 fleet-relayd.service 顶部说明），为了读 root 0600 的 Xray 配置
if [ "$KIND" = relay ]; then BIN=relayd SVC=fleet-relayd SVC_USER=root; else BIN=nodeagent SVC=fleet-nodeagent SVC_USER=fleet-edge; fi
log() { printf '[%s] %s: %s\n' "$(date +%T)" "$HOST_ID" "$*"; }
die() { log "失败：$*" >&2; exit 1; }

# ---------- SSH ----------
CP_DIR=/tmp/fe-ssh-$(id -u)
mkdir -p "$CP_DIR"; chmod 700 "$CP_DIR"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15 -o ControlMaster=auto -o "ControlPath=$CP_DIR/%C" -o ControlPersist=60)
if [ "$JUMP" = direct ]; then
  SSH+=("root@$IP")
else
  if [ -z "$KEY" ] && [ -f "$INVENTORY" ]; then
    KEY=$(awk -v ip="$IP" '$2==ip {print $4; exit}' "$INVENTORY")
  fi
  KEY=${KEY:-~/.ssh/id_ed25519_38_58_57_22}
  KEY=${KEY/#\~/$HOME}
  [ -f "$KEY" ] || die "找不到私钥 $KEY"
  SSH+=(-o IdentitiesOnly=yes -i "$KEY" -J "$JUMP" "root@$IP")
fi
rsh() { "${SSH[@]}" "$@"; }

# ---------- 卸载 ----------
if [ "$MODE" = remove ]; then
  log "卸载 fleet-edge"
  rsh 'bash -s' <<'EOS'
set -u
for s in fleet-nodeagent fleet-relayd; do
  systemctl disable --now $s.service >/dev/null 2>&1 || true
  rm -f /etc/systemd/system/$s.service
  rm -rf /etc/systemd/system/$s.service.d /var/lib/$s /var/lib/private/$s
done
systemctl daemon-reload
systemctl reset-failed fleet-nodeagent.service fleet-relayd.service >/dev/null 2>&1 || true
rm -rf /opt/fleet-edge /etc/fleet-edge
id fleet-edge >/dev/null 2>&1 && userdel fleet-edge 2>/dev/null || true
getent group fleet-edge >/dev/null && groupdel fleet-edge 2>/dev/null || true
echo "remaining: $(ls -d /opt/fleet-edge /etc/fleet-edge /var/lib/fleet-* 2>/dev/null | wc -l) paths, unit: $(systemctl list-unit-files 'fleet-*' --no-legend 2>/dev/null | wc -l)"
EOS
  if [ "$REVOKE" = 1 ]; then
    log "在中控撤销令牌"; eval "$(printf "$REVOKE_CMD" "$HOST_ID")" || log "撤销失败（可稍后手动执行）"
  fi
  log "已卸载"; exit 0
fi

# ---------- 安装 / 升级 ----------
[ -x "$DIST/$BIN" ] || die "找不到 $DIST/$BIN（先运行 deploy/fetch-dist.sh）"
SHA=$(shasum -a 256 "$DIST/$BIN" | cut -d' ' -f1)
VER=$(cat "$DIST/VERSION" 2>/dev/null || echo 0.1.0)-${SHA:0:8}
REL=/opt/fleet-edge/releases/$VER

log "预检（$BIN $VER）"
PRE=$(rsh "bash -s" <<EOS
set -u
echo "arch=\$(uname -m)"
echo "systemd=\$(systemctl --version | awk 'NR==1{print \$2}')"
echo "cgroup=\$(stat -fc %T /sys/fs/cgroup 2>/dev/null)"
echo "os=\$(. /etc/os-release; echo \$ID-\$VERSION_ID)"
if timeout 6 bash -c 'exec 3<>/dev/tcp/${ENDPOINT#https://}/443' 2>/dev/null; then echo out443=ok; else echo out443=fail; fi
echo "have_token=\$([ -s /etc/fleet-edge/token ] && echo 1 || echo 0)"
echo "have_bin=\$([ -x $REL/$BIN ] && sha256sum $REL/$BIN | cut -c1-64)"
echo "mem_avail_mb=\$(awk '/MemAvailable/{print int(\$2/1024)}' /proc/meminfo)"
echo "xcfg=\$([ -f $XCONFIG ] && echo 1 || echo 0)"
echo "fleet_xray=\$(systemctl is-active fleet-xray.service 2>/dev/null | head -1)"
echo "komari_group=\$(getent group komari >/dev/null && echo 1 || echo 0)"
EOS
) || die "SSH 失败"
eval "$(echo "$PRE" | grep -E '^[a-z_0-9]+=' )"
log "$(echo $PRE | tr '\n' ' ')"
[ "$arch" = x86_64 ] || die "架构 $arch 不支持"
if [ "$KIND" = relay ] && [ "$xcfg" != 1 ]; then log "警告：$XCONFIG 不存在（切换还没做？）；relayd 会退回 127.0.0.1:62789 且没有探测对象"; fi
[ "${systemd:-0}" -ge 239 ] 2>/dev/null || die "systemd $systemd 太旧"
if [ "$out443" != ok ]; then [ $FORCE = 1 ] && log "警告：到 ${ENDPOINT#https://}:443 的出站不通（--force 继续）" || die "到 ${ENDPOINT#https://}:443 的出站不通（nftables？）；--force 可跳过"; fi

# 令牌
TOKEN=
if [ -n "$TOKFILE" ]; then
  TOKEN=$(tr -d '[:space:]' < "$TOKFILE")
elif [ "$have_token" = 1 ] && [ $ROTATE = 0 ]; then
  log "沿用目标机上已有的令牌"
else
  log "在中控签发令牌"
  TOKEN=$(eval "$(printf "$TOKEN_CMD" "$HOST_ID")") || die "签发令牌失败"
  TOKEN=$(printf '%s' "$TOKEN" | tr -d '[:space:]')
fi
[ -z "$TOKEN" ] || [ ${#TOKEN} -ge 20 ] || die "令牌格式不对"

# 用户与目录
rsh "bash -s" <<'EOS' || die "创建用户/目录失败"
set -eu
getent group fleet-edge >/dev/null || groupadd --system fleet-edge
id fleet-edge >/dev/null 2>&1 || useradd --system --gid fleet-edge --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-edge
install -d -m 0755 -o root -g root /opt/fleet-edge /opt/fleet-edge/releases /etc/fleet-edge
EOS

# 二进制
if [ "$have_bin" != "$SHA" ]; then
  log "上传 $BIN（$(du -h "$DIST/$BIN" | cut -f1)）"
  rsh "set -e; install -d -m 0755 $REL; cat > $REL/$BIN.tmp; echo '$SHA  $REL/$BIN.tmp' | sha256sum -c --quiet; chmod 0755 $REL/$BIN.tmp; mv -f $REL/$BIN.tmp $REL/$BIN" < "$DIST/$BIN" || die "上传失败"
else
  log "二进制已是 $VER"
fi

# 配置
CONF=$(cat <<EOF
# 由 fleet-edge/deploy/rollout.sh 生成（内容不变就不重启）
endpoint = "$ENDPOINT"
host_id = "$HOST_ID"
token_file = "/etc/fleet-edge/token"
sample_interval_s = $INTERVAL
batch_samples = $BATCHN
EOF
)
if [ "$KIND" = relay ]; then
  CONF+=$(printf '\noutbox_max_bytes = 16777216\n\n[xray]\nconfig = "%s"\nbin = "%s"\nproc_match = "%s"\nseed_file = "/etc/fleet-edge/xray-seed.json"\n' "$XCONFIG" "$XBIN" "$XMATCH")
  [ -n "$XAPI" ] && CONF+=$(printf '\napi = "%s"' "$XAPI")
  CONF+=$(printf '\n\n[probe]\nenabled = %s\npath = %s\n' "$([ $PROBE = 1 ] && echo true || echo false)" "$([ $PATHCHK = 1 ] && echo true || echo false)")
  CONF+=$(printf '\n\n[manage]\nenabled = %s\nconfig = "%s"\n' "$([ $MANAGE = 1 ] && echo true || echo false)" "$XCONFIG")
  CONF+=$(printf '\n\n[access]\nenabled = %s\n' "$([ $ACCESS = 1 ] && echo true || echo false)")
  if [ -n "$RUUID" ] && [ -n "$RIP" ]; then
    PL=""; for p in "${RPEERS[@]}"; do PL+="${PL:+, }\"$p\""; done
    CONF+=$(printf '\n\n[relay]\nasset_uuid = "%s"\npublic_ip = "%s"\npeers = [%s]\n' "$RUUID" "$RIP" "$PL")
    [ -n "$RMPORT" ] && CONF+=$(printf '\nmaster_port = %s' "$RMPORT")
  fi
fi
# 配置里有资产 uuid 与互探对象：0600，属服务用户（relayd → root，nodeagent → fleet-edge）；内容没变也把旧的 0644 收紧（不触发重启）
printf '%s\n' "$CONF" | rsh "umask 077; cat > /etc/fleet-edge/agent.toml.new && chown $SVC_USER:$SVC_USER /etc/fleet-edge/agent.toml.new && chmod 0600 /etc/fleet-edge/agent.toml.new && if cmp -s /etc/fleet-edge/agent.toml.new /etc/fleet-edge/agent.toml; then rm -f /etc/fleet-edge/agent.toml.new; else mv -f /etc/fleet-edge/agent.toml.new /etc/fleet-edge/agent.toml; echo changed > /etc/fleet-edge/.restart; fi && chown $SVC_USER:$SVC_USER /etc/fleet-edge/agent.toml && chmod 0600 /etc/fleet-edge/agent.toml && stat -c 'agent.toml %a %U:%G' /etc/fleet-edge/agent.toml" || die "写配置失败"
if [ -n "$TOKEN" ]; then
  printf '%s\n' "$TOKEN" | rsh "umask 077; cat > /etc/fleet-edge/token.new && chown $SVC_USER:$SVC_USER /etc/fleet-edge/token.new && chmod 0600 /etc/fleet-edge/token.new && mv -f /etc/fleet-edge/token.new /etc/fleet-edge/token && echo changed > /etc/fleet-edge/.restart" || die "写令牌失败"
  log "令牌已写入（0600 $SVC_USER）"
fi
unset TOKEN
if [ -n "$SEED" ]; then
  python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert isinstance(d["ts"],(int,float)) and isinstance(d["entries"],list)' "$SEED" || die "种子文件格式不对"
  rsh "cat > /etc/fleet-edge/xray-seed.json.new && chown root:root /etc/fleet-edge/xray-seed.json.new && chmod 0600 /etc/fleet-edge/xray-seed.json.new && mv -f /etc/fleet-edge/xray-seed.json.new /etc/fleet-edge/xray-seed.json" < "$SEED" || die "上传种子失败"
  log "种子已上传（relayd 下一批时应用）"
fi

# 单元
rsh "cat > /etc/systemd/system/$SVC.service.new && if cmp -s /etc/systemd/system/$SVC.service.new /etc/systemd/system/$SVC.service; then rm -f /etc/systemd/system/$SVC.service.new; else mv -f /etc/systemd/system/$SVC.service.new /etc/systemd/system/$SVC.service; echo changed > /etc/fleet-edge/.restart; fi" < "$HERE/$SVC.service" || die "写单元失败"
DROPIN="# 由 rollout.sh 生成"
if [ "$cgroup" != cgroup2fs ]; then
  # cgroup v1（Ubuntu 20.04 等）：MemoryMax 不生效，补 MemoryLimit
  if [ "$KIND" = relay ]; then DROPIN+=$'\n[Service]\nMemoryLimit=128M'; else DROPIN+=$'\n[Service]\nMemoryLimit=24M'; fi
fi
GROUPS_=""
[ -n "$XGROUP" ] && [ "$KIND" = relay ] && GROUPS_="$XGROUP"
# 过渡期：relayd 只读转发 /var/lib/komari-relay-probe/status.json（目录 komari:komari 0750；relayd 是无 capability 的 root，进不去）
[ "$KIND" = relay ] && [ "${komari_group:-0}" = 1 ] && GROUPS_="${GROUPS_:+$GROUPS_ }komari"
if [ -n "$GROUPS_" ]; then
  grep -q '^\[Service\]' <<<"$DROPIN" || DROPIN+=$'\n[Service]'
  DROPIN+=$'\n'"SupplementaryGroups=$GROUPS_"
fi
printf '%s\n' "$DROPIN" | rsh "install -d -m 0755 /etc/systemd/system/$SVC.service.d && cat > /etc/systemd/system/$SVC.service.d/10-local.conf.new && if cmp -s /etc/systemd/system/$SVC.service.d/10-local.conf.new /etc/systemd/system/$SVC.service.d/10-local.conf; then rm -f /etc/systemd/system/$SVC.service.d/10-local.conf.new; else mv -f /etc/systemd/system/$SVC.service.d/10-local.conf.new /etc/systemd/system/$SVC.service.d/10-local.conf; echo changed > /etc/fleet-edge/.restart; fi"

# 以服务用户干跑一次（校验配置、令牌可读、采样正常），再切换 current 并启动
log "干跑 --once（以 $SVC_USER 身份）"
rsh "runuser -u $SVC_USER -- $REL/$BIN --once --config /etc/fleet-edge/agent.toml >/dev/null 2>/tmp/fe-once.err; rc=\$?; grep -q '令牌文件可读' /tmp/fe-once.err || { cat /tmp/fe-once.err; rc=9; }; rm -f /tmp/fe-once.err; exit \$rc" || die "干跑失败（配置或令牌不可读）"

START_OUT=$(rsh "bash -s" <<EOS
set -eu
echo "t0=\$(date +%s)"
cur=\$(readlink /opt/fleet-edge/current 2>/dev/null || true)
if [ "\$cur" != "$REL" ]; then ln -sfn $REL /opt/fleet-edge/current.new && mv -Tf /opt/fleet-edge/current.new /opt/fleet-edge/current; echo changed > /etc/fleet-edge/.restart; fi
systemctl daemon-reload
systemctl enable $SVC.service >/dev/null 2>&1
if [ -f /etc/fleet-edge/.restart ] || ! systemctl is-active --quiet $SVC.service; then systemctl restart $SVC.service; echo restarted; else echo unchanged; fi
rm -f /etc/fleet-edge/.restart
# 只留最近 3 个版本
ls -1dt /opt/fleet-edge/releases/* 2>/dev/null | tail -n +4 | grep -v "^$REL\$" | xargs -r rm -rf
EOS
) || die "启动失败"
T0=$(printf '%s\n' "$START_OUT" | awk -F= '/^t0=/{print $2}')
log "服务：$(printf '%s' "$START_OUT" | grep -v '^t0=' | tr '\n' ' ')"

if [ $WAIT = 1 ]; then
  log "等待第一批被中控接收（最多 120 s）"
  for i in $(seq 40); do
    sleep 3
    ST=$(rsh "cat /var/lib/$SVC/status.json 2>/dev/null" || true)
    LAST_OK=$(printf '%s' "$ST" | python3 -c 'import json,sys
try: print(int(json.load(sys.stdin).get("last_ok",0)))
except Exception: print(0)')
    if [ "${LAST_OK:-0}" -ge "$T0" ]; then log "已接收：$ST"; break; fi
    if [ $i = 40 ]; then
      log "120 s 内没有被接收的批次；status=$ST"
      rsh "journalctl -u $SVC.service -n 15 --no-pager" || true
      exit 3
    fi
  done
  rsh "systemctl show $SVC.service -p MainPID -p MemoryCurrent -p CPUUsageNSec -p NRestarts" | tr '\n' ' '; echo
fi
log "完成（$BIN $VER）"
