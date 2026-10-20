#!/bin/bash
# enable-onboard.sh — 在中控上启用 fleetd 内嵌的上架流水线（/v2/api/v1/onboard/*）与 fleetctl。以 root 运行，幂等。
#
#   bash enable-onboard.sh <fleetctl> <shadowsocks-…-musl.tar.xz>   （同目录要有 .sha256 与 onboard.central.toml）
#
# 做的事：/etc/fleet/onboard（密钥：节点用 id_ed25519、线路机用 relay_ed25519，fleetd 所有 0600）、relays.known_hosts
# （取 root 已信任的 known_hosts；本机 127.0.0.1 用 keyscan）、/var/lib/fleet/dist 放 ssserver 包、/var/lib/fleetd/onboard、
# /etc/fleet/onboard.toml、/usr/local/bin/fleetctl、fleetd.toml 加 onboard_config、本机 root 授权 relay 公钥（from= 限定）。
# 线路机 line1 / line3 的 root authorized_keys 需要另外加上最后打印的那一行。之后 upgrade-central.sh 或 systemctl restart fleetd。
set -euo pipefail
FLEETCTL=${1:?用法: enable-onboard.sh <fleetctl> <ss tarball>}
TARBALL=${2:?用法: enable-onboard.sh <fleetctl> <ss tarball>}
HERE=$(cd "$(dirname "$0")" && pwd)
D=/etc/fleet/onboard
TS=$(date +%Y%m%d%H%M%S)
[ "$(id -u)" = 0 ] || { echo "需要 root" >&2; exit 1; }
id fleetd >/dev/null || { echo "没有 fleetd 用户（先装 fleetd）" >&2; exit 1; }
[ -f "$TARBALL.sha256" ] || { echo "缺 $TARBALL.sha256" >&2; exit 1; }

echo "== 密钥 $D"
install -d -m 750 -o root -g fleetd /etc/fleet "$D"
chmod 755 /etc/fleet
for k in id_ed25519:fleet-onboard-node relay_ed25519:fleet-onboard-relay; do
  f=${k%%:*}; c=${k##*:}
  [ -f "$D/$f" ] || ssh-keygen -q -t ed25519 -N '' -C "$c" -f "$D/$f"
  chown fleetd:fleetd "$D/$f" "$D/$f.pub"; chmod 600 "$D/$f"; chmod 644 "$D/$f.pub"
done

echo "== relays.known_hosts"
KH=$D/relays.known_hosts.new
: > "$KH"
for ip in 23.148.204.129 38.58.57.69; do
  if ssh-keygen -F "$ip" -f /root/.ssh/known_hosts >/dev/null 2>&1; then
    ssh-keygen -F "$ip" -f /root/.ssh/known_hosts | grep -v '^#' >> "$KH"
  else
    echo "警告：root 的 known_hosts 里没有 $ip，改用 keyscan（请核对指纹）"; ssh-keyscan -t ed25519 "$ip" 2>/dev/null | tee -a "$KH" | ssh-keygen -lf - || true
  fi
done
ssh-keyscan -t ed25519 127.0.0.1 2>/dev/null >> "$KH"
install -m 640 -o root -g fleetd "$KH" "$D/relays.known_hosts"; rm -f "$KH"
ssh-keygen -lf "$D/relays.known_hosts" | sed 's/^/  /'

echo "== ssserver 包 /var/lib/fleet/dist"
install -d -m 755 /var/lib/fleet /var/lib/fleet/dist
install -m 644 "$TARBALL" "$TARBALL.sha256" /var/lib/fleet/dist/
(cd /var/lib/fleet/dist && sha256sum -c "$(basename "$TARBALL").sha256")

echo "== 状态目录与配置"
install -d -m 750 -o fleetd -g fleetd /var/lib/fleetd/onboard /var/lib/fleetd/onboard/work
if [ ! -f /etc/fleet/onboard.toml ]; then
  install -m 640 -o root -g fleetd "$HERE/onboard.central.toml" /etc/fleet/onboard.toml
else
  echo "/etc/fleet/onboard.toml 已存在，不覆盖"
fi
install -m 755 "$FLEETCTL" /usr/local/bin/fleetctl

echo "== fleetd.toml onboard_config"
if ! grep -q '^onboard_config' /etc/fleetd/fleetd.toml; then
  cp -p /etc/fleetd/fleetd.toml /etc/fleetd/fleetd.toml.before-onboard-$TS
  python3 - <<'PY'
p = "/etc/fleetd/fleetd.toml"
lines = open(p).read().split("\n")
i = next(n for n, l in enumerate(lines) if l.startswith("["))
lines.insert(i, 'onboard_config = "/etc/fleet/onboard.toml"   # 上架流水线（/v2/api/v1/onboard/*）\n')
open(p + ".new", "w").write("\n".join(lines))
PY
  chown root:fleetd /etc/fleetd/fleetd.toml.new; chmod 640 /etc/fleetd/fleetd.toml.new
  mv /etc/fleetd/fleetd.toml.new /etc/fleetd/fleetd.toml
fi
runuser -u fleetd -- /opt/fleetd/current/fleetd check-config --config /etc/fleetd/fleetd.toml | grep -E "onboard|OK" || true

echo "== 本机（线路机2）root 授权 relay 公钥（只接受来自本机）"
LINE="from=\"127.0.0.1,38.58.57.22\" $(cat $D/relay_ed25519.pub)"
grep -qF "$(cut -d' ' -f2 $D/relay_ed25519.pub)" /root/.ssh/authorized_keys || echo "$LINE" >> /root/.ssh/authorized_keys
echo
echo "line1 / line3 的 /root/.ssh/authorized_keys 需要加上：from=\"38.58.57.22\" $(cat $D/relay_ed25519.pub)"
