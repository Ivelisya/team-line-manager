#!/bin/bash
# 在 PVE 上建实验室软路由 lab-router1：OpenWrt 24.10 x86_64 的 LXC 容器 170，静态 192.168.31.70，
# 模拟生产的软路由（iStoreOS + OpenClash），给「软路由纳入面板」做端到端测试。说明见 lab/router/README.md。
# 幂等：模板已下载就只校验、容器已有就不重建，其余每次按仓库里的文件修正；出口每次按实验室中控的现状重新生成。
# 只动 PVE 上的容器 170 与 /root/lab-router；别的虚拟机、容器一概不碰。
# root 密码随机，存 fleet-dev:/root/lab-root-passwords/lab-router1（0600）；OpenClash 的 secret 只在路由的 uci 里。都不打印。
# 用法：lab/mkrouter.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
F="-F $HOME/.ssh/fleetlab_config"
ID=170
PVE() { ssh -o BatchMode=yes pve "$@"; }
DEV() { ssh $F fleet-dev "$@"; }

echo "== 1. PVE：官方 rootfs、容器 $ID"
tar -C "$HERE/router" -czf - . | PVE 'rm -rf /root/lab-router/src && mkdir -p /root/lab-router/src && tar --no-same-owner -xzf - -C /root/lab-router/src'
PVE "ID=$ID bash -s" <<'PVE'
set -euo pipefail
V=24.10.8
F=openwrt-$V-x86-64-rootfs.tar.gz
T=/var/lib/vz/template/cache/$F
D=/root/lab-router/dl
# 只从官方 releases 目录下载，按同目录的 sha256sums 校验后才用（已下载的每次也重新校验）
if [ ! -f "$D/sha256sums" ] || [ ! -f "$T" ]; then
  mkdir -p "$D"
  B=https://downloads.openwrt.org/releases/$V/targets/x86/64
  curl -fsS --max-time 60 -o "$D/sha256sums" "$B/sha256sums"
  curl -fsS --max-time 600 -o "$D/$F" "$B/$F"
  (cd "$D" && grep " \*$F\$" sha256sums | sha256sum -c --quiet -)
  mv "$D/$F" "$T"
fi
(cd "$(dirname "$T")" && grep " \*$F\$" "$D/sha256sums" | sha256sum -c --quiet -)
# OpenClash 核心：生产上拷来的同一个文件（mihomo alpha-g99e68e9），不从别处下载
echo "76c21f0084ac93341e92686daa39b3276b0336566e722ad83a7899050edbe50b  /root/lab-router/clash_meta" | sha256sum -c --quiet -
if pct status "$ID" >/dev/null 2>&1; then
  echo "容器 $ID 已有，不重建"
else
  # ostype unmanaged：PVE 不改容器里的网络与主机名，全由 OpenWrt 自己的 uci 管；非特权容器里 procd、netifd、fw4 都能跑
  pct create "$ID" "local:vztmpl/$F" --arch amd64 --ostype unmanaged --hostname lab-router1 \
    --cores 2 --memory 512 --swap 256 --rootfs local-lvm:1 --net0 name=eth0,bridge=vmbr0 \
    --unprivileged 1 --onboot 0 --tags fleetlab \
    --description "fleet lab router: OpenWrt $V, 192.168.31.70 (Claude, lab/mkrouter.sh)" >/dev/null
  # 第一次启动前写好网络与系统设置：eth0 接的是家里的局域网，OpenWrt 默认会把唯一的网口当 lan 发 DHCP / RA
  pct mount "$ID" >/dev/null
  for f in network system; do
    install -m 644 -o 100000 -g 100000 "/root/lab-router/src/uci/$f" "/var/lib/lxc/$ID/rootfs/etc/config/$f"
  done
  pct unmount "$ID"
  echo "已建容器 $ID"
fi
pct status "$ID" | grep -q running || pct start "$ID"
for _ in $(seq 30); do pct exec "$ID" -- /bin/ubus call system board >/dev/null 2>&1 && break; sleep 1; done
# 仓库里的文件与核心放进容器的 /tmp/lab-router，setup.sh 从这里装
tar -C /root/lab-router -cf - clash_meta -C src . | pct exec "$ID" -- sh -c 'rm -rf /tmp/lab-router && mkdir -p /tmp/lab-router && tar -xf - -C /tmp/lab-router'
PVE

echo "== 2. 路由：软件包、登录、模拟 OpenClash、router-tunnel 空壳、流量发生器"
DEV 'cat /etc/fleet/ssh/id_ed25519.pub
     f=/root/lab-root-passwords/lab-router1
     [ -s "$f" ] || { install -d -m 700 /root/lab-root-passwords; (umask 077; head -c 18 /dev/urandom | base64 | tr -d "/+=" > "$f"); }
     cat "$f"' | PVE "pct exec $ID -- sh /tmp/lab-router/setup.sh"
PVE "pct exec $ID -- rm -rf /tmp/lab-router"

echo "== 3. 出口：实验室中控里在用的入口（admin 会话取分享链接，同 lab/e2e.sh；链接直接经管道交给路由，不落盘、不打印）"
DEV 'bash -s' <<'REMOTE' | PVE "pct exec $ID -- sh /usr/share/lab-router/gen-exits.sh"
set -euo pipefail
B=http://127.0.0.1:8080/api/v1
J=$(mktemp /tmp/lab-router.cookies.XXXXXX)   # admin 的会话 cookie：随机名、0600
api() { curl -fsS -b "$J" -H 'x-fleet-request: 1' "$@"; }
trap 'api -X POST "$B/auth/logout" >/dev/null 2>&1 || true; rm -f "$J"' EXIT   # 用完退出登录，不留会话
jq -n --rawfile p /root/fleet-admin-password '{username:"admin",password:($p|rtrimstr("\n"))}' \
  | curl -fsS -c "$J" -H 'content-type: application/json' -d @- "$B/auth/login" >/dev/null
# 实验室真实线路机（192.168.31.x；模拟器的假机器在 10.77.0.0/16）上在用的入口，按线路机名、端口排，最多 6 条
relays=$(api "$B/machines?view=brief" | jq -r '.data[] | select(.kind == "relay" and .lifecycle == "active" and (.ip // "" | startswith("192.168.31."))) | .id')
for r in $relays; do
  api "$B/machines/$r" | jq -c '.data.entries[] | select(.lifecycle == "active") | {id, relay: .relay.name, port, node: .node.name}'
done | jq -s -c 'sort_by(.relay, .port) | .[:6] | .[]' | while read -r e; do
  url=$(api "$B/entries/$(jq -r .id <<<"$e")/share" | jq -r .data.url)
  printf '%s %s\n' "$url" "$(jq -r '"\(.relay):\(.port)→\(.node)"' <<<"$e")"
done
REMOTE
# reload：运行配置变了才重启核心（重启会断掉所有连接）；流量发生器总是（重新）起一次，马上按新规则建好连接
PVE "pct exec $ID -- sh -c '/etc/init.d/openclash reload && if /etc/init.d/lab-traffic running; then /etc/init.d/lab-traffic restart; else /etc/init.d/lab-traffic start; fi'"

echo "== 4. 自检"
sleep 10   # 等流量发生器的第一轮连接
PVE "pct exec $ID -- sh /usr/share/lab-router/check.sh"
echo "登录：fleet-dev 上 ssh -i /etc/fleet/ssh/id_ed25519 root@192.168.31.70；PVE 上 pct enter $ID"
