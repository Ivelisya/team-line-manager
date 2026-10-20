#!/bin/bash
# rollback.sh — 以 root 在中控运行。平时用 --previous（切回上一版本）；不带参数是 1a 时期的整体回退（停 fleetd + 删 /v2/api 路由），旧系统已下线后不要用。
#   bash rollback.sh            停服务 + 删路由（数据 /var/lib/fleetd 与 /opt/fleetd 保留，可随时重新 install）
#   bash rollback.sh --previous 切回上一个已安装版本并重启（不动 Caddy）
#   --previous 时若上一版本不接受当前配置，会先换回最近一份它接受的 /etc/fleetd/fleetd.toml.before-* 备份（当前配置另存 .rolledback-<ts>）。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
[ "$(id -u)" = 0 ] || { echo "需要 root" >&2; exit 1; }
if [ "${1:-}" = "--previous" ]; then
  cd /opt/fleetd/releases
  CUR=$(basename "$(readlink /opt/fleetd/current)")
  PREV=$(ls -1 | sort | grep -v "^$CUR$" | tail -n 1)
  [ -n "$PREV" ] || { echo "没有上一个版本" >&2; exit 1; }
  # 上一版本不认当前配置（新版本加的配置值，例如 auth.mode = "fleetd"）→ 换回最近一份它认的 fleetd.toml.before-* 备份
  CFG=/etc/fleetd/fleetd.toml
  if ! runuser -u fleetd -- "/opt/fleetd/releases/$PREV/fleetd" check-config --config "$CFG" >/dev/null 2>&1; then
    FOUND=
    for b in $(ls -1t /etc/fleetd/fleetd.toml.before-* 2>/dev/null); do
      if runuser -u fleetd -- "/opt/fleetd/releases/$PREV/fleetd" check-config --config "$b" >/dev/null 2>&1; then
        TS=$(date +%Y%m%d%H%M%S)
        cp -p "$CFG" "$CFG.rolledback-$TS" && cp -p "$b" "$CFG"
        echo "上一版本不接受当前配置：已换回 $b（当前配置留在 $CFG.rolledback-$TS）"
        FOUND=1
        break
      fi
    done
    [ -n "$FOUND" ] || { echo "上一版本（$PREV）不接受当前配置，也没有它能接受的 fleetd.toml.before-* 备份，未回退" >&2; exit 1; }
  fi
  ln -sfn "releases/$PREV" /opt/fleetd/current.new && mv -Tf /opt/fleetd/current.new /opt/fleetd/current
  systemctl restart fleetd
  echo "current -> releases/$PREV（原 $CUR），已重启"
  exit 0
fi
bash "$HERE/caddy-route.sh" --remove
systemctl disable --now fleetd 2>/dev/null || true
echo "fleetd 已停用；Caddy 路由已移除。数据保留在 /var/lib/fleetd、/opt/fleetd、/etc/fleetd。"
