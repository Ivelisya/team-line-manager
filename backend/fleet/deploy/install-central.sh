#!/bin/bash
# install-central.sh — 在中控（38.58.57.22，Ubuntu 20.04）安装或升级 fleetd。以 root 运行，幂等。
#
#   bash install-central.sh <fleetd 二进制> [catalog.json]
#
# 在同一目录需要：fleetd.service、fleetd.toml.example（本脚本旁边的文件）。
# 做的事：
#   1. 建系统用户 fleetd（无登录 shell）；
#   2. 二进制装到 /opt/fleetd/releases/<ts>/fleetd（+ catalog.json），current 符号链接原子切换，保留最近 5 个版本；
#   3. /etc/fleetd/fleetd.toml 不存在时从 fleetd.toml.example 生成（已存在则不动）；
#   4. 旧后端 key：/var/lib/komari/data/plugin/team-ops/backend-key.json 的 "key" 字段
#      （回退 /etc/team-ops/backend.json 的 "key"）→ /etc/fleetd/legacy.key，0640 root:fleetd；
#   5. 装 systemd unit，check-config，enable + restart；
#   6. 用 curl 在 127.0.0.1:25900 核对 /v2/api/v1/system。
# 不改旧系统、Komari、Caddy（Caddy 路由另用 caddy-route.sh）。
set -euo pipefail
BIN=${1:?用法: install-central.sh <fleetd 二进制> [catalog.json]}
CATALOG=${2:-}
HERE=$(cd "$(dirname "$0")" && pwd)
TS=$(date +%Y%m%d%H%M%S)
PREFIX=/opt/fleetd
ETC=/etc/fleetd
UNIT=/etc/systemd/system/fleetd.service
KEY_SRC=/var/lib/komari/data/plugin/team-ops/backend-key.json
KEY_FALLBACK=/etc/team-ops/backend.json

say() { echo "== $*"; }
die() { echo "错误：$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "需要 root"
[ -f "$BIN" ] || die "找不到二进制 $BIN"
for f in fleetd.service fleetd.toml.example fetch-dbs.sh fleetd-ipintel-fetch.service fleetd-ipintel-fetch.timer; do
  [ -f "$HERE/$f" ] || die "$f 不在 $HERE"
done
chmod 755 "$BIN"
"$BIN" version >/dev/null || die "$BIN 无法运行（应为 x86_64 静态二进制）"
say "版本：$("$BIN" version)"

say "用户 fleetd"
if ! id fleetd >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/fleetd --no-create-home --shell /usr/sbin/nologin fleetd
fi

say "安装到 $PREFIX/releases/$TS"
install -d -m 755 "$PREFIX/releases/$TS"
install -m 755 "$BIN" "$PREFIX/releases/$TS/fleetd"
if [ -n "$CATALOG" ]; then
  install -m 644 "$CATALOG" "$PREFIX/releases/$TS/catalog.json"
elif [ -f "$PREFIX/current/catalog.json" ]; then
  install -m 644 "$PREFIX/current/catalog.json" "$PREFIX/releases/$TS/catalog.json"
fi
install -d -m 755 "$PREFIX/releases/$TS/ipintel"
install -m 644 "$HERE/fetch-dbs.sh" "$PREFIX/releases/$TS/ipintel/fetch-dbs.sh"
PREV=$(readlink "$PREFIX/current" 2>/dev/null || true)
ln -sfn "releases/$TS" "$PREFIX/current.new" && mv -Tf "$PREFIX/current.new" "$PREFIX/current"
(cd "$PREFIX/releases" && ls -1 | sort | head -n -5 | xargs -r rm -rf)
echo "current -> releases/$TS（之前：${PREV:-无}）"

say "配置 $ETC"
install -d -m 750 -o root -g fleetd "$ETC"
if [ ! -f "$ETC/fleetd.toml" ]; then
  install -m 640 -o root -g fleetd "$HERE/fleetd.toml.example" "$ETC/fleetd.toml"
  echo "已生成 $ETC/fleetd.toml（影子模式，未配置通知渠道）"
else
  echo "$ETC/fleetd.toml 已存在，不覆盖"
fi

say "旧后端 key → $ETC/legacy.key"
SRC=$KEY_SRC
[ -f "$SRC" ] || SRC=$KEY_FALLBACK
[ -f "$SRC" ] || die "找不到旧后端 key（$KEY_SRC / $KEY_FALLBACK）"
umask 077
python3 - "$SRC" "$ETC/legacy.key.new" <<'PY'
import json, sys
key = json.load(open(sys.argv[1]))["key"]
assert isinstance(key, str) and len(key) >= 16, "key 字段无效"
open(sys.argv[2], "w").write(key + "\n")
PY
chown root:fleetd "$ETC/legacy.key.new" && chmod 640 "$ETC/legacy.key.new" && mv -f "$ETC/legacy.key.new" "$ETC/legacy.key"
echo "来自 $SRC（字段 key）"
umask 022

say "systemd unit"
install -m 644 "$HERE/fleetd.service" "$UNIT"
install -m 644 "$HERE/fleetd-ipintel-fetch.service" /etc/systemd/system/fleetd-ipintel-fetch.service
install -m 644 "$HERE/fleetd-ipintel-fetch.timer" /etc/systemd/system/fleetd-ipintel-fetch.timer
install -d -m 700 -o fleetd -g fleetd /var/lib/fleetd
systemctl daemon-reload

say "IP 情报离线库（/var/lib/fleetd/ipintel，约 230 MB；失败不阻塞 fleetd）"
if [ ! -f /var/lib/fleetd/ipintel/MANIFEST ]; then
  AVAIL=$(df -Pm /var/lib | awk 'NR==2 {print $4}')
  if [ "${AVAIL:-0}" -lt 600 ]; then
    echo "警告：/var/lib 只剩 ${AVAIL} MB，跳过下载（需要约 400 MB 余量）"
  else
    systemctl start fleetd-ipintel-fetch.service || journalctl -u fleetd-ipintel-fetch -n 30 --no-pager || true
  fi
else
  echo "已存在，跳过（每月由 timer 更新）"
fi
systemctl enable --now fleetd-ipintel-fetch.timer >/dev/null 2>&1 || true
runuser -u fleetd -- "$PREFIX/current/fleetd" check-config --config "$ETC/fleetd.toml"
systemctl enable fleetd >/dev/null 2>&1
systemctl restart fleetd

say "核对 http://127.0.0.1:25900/v2/api/v1/system"
for i in $(seq 1 30); do
  if OUT=$(curl -fsS -m 3 http://127.0.0.1:25900/v2/api/v1/system 2>/dev/null); then
    python3 - "$OUT" <<'PY'
import json, sys
d = json.loads(sys.argv[1])["data"]
polls = {p["resource"]: p for p in d["polls"]}
ov = polls.get("overview", {})
print("fleetd %s  pid %s  rss %.1f MB  cadence %s  shadow %s" % (d["version"], d["pid"], d["rss_mb"], d["cadence"], d["notify"]["shadow"]))
print("legacy %s  overview v%s age %s s  errors %s  last_error %s" % (d["legacy"]["base_url"], ov.get("version"), ov.get("age_s"), ov.get("errors"), ov.get("last_error")))
print("komari reachable %s  db %s bytes" % (d["komari"]["reachable"], d["db"]["size_bytes"]))
PY
    systemctl --no-pager --lines=0 status fleetd | head -5
    echo "OK：fleetd 已运行（回滚：bash rollback.sh；Caddy 路由：bash caddy-route.sh）"
    exit 0
  fi
  sleep 1
done
journalctl -u fleetd --no-pager -n 30
die "fleetd 30 s 内没有响应"
