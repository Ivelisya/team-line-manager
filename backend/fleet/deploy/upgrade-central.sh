#!/bin/bash
# upgrade-central.sh — 升级中控上已安装的 fleetd。以 root 运行。
#
#   bash upgrade-central.sh <新 fleetd 二进制> [catalog.json]
#
# 保留 /etc/fleetd（配置、key）与 /var/lib/fleetd（数据库）不动；只做：
#   1. 新二进制自检（version、用现有配置 check-config）；不通过就不动线上；
#   2. 新版本目录 /opt/fleetd/releases/<ts>（二进制 + catalog + ipintel/fetch-dbs.sh），更新 systemd unit
#      （fleetd.service、fleetd-ipintel-fetch.{service,timer}）；
#   3. IP 情报离线库缺失时先下载一次（fetch-dbs，约 1 分钟；失败不阻塞），并启用每月更新的 timer；
#   4. current 原子切换、重启 fleetd；30 s 内 /v2/api/v1/system 必须返回 200 且版本号正确，
#      否则自动切回上一个版本（上一版本不认当前配置时先换回最近一份它认的 fleetd.toml.before-* 备份）、重启并退出非 0。
# 要随这次升级一起改 /etc/fleetd/fleetd.toml：先 cp -p 留 fleetd.toml.before-<阶段>-<时间戳>，再改，再跑本脚本（自检用的是新二进制）。
set -euo pipefail
BIN=${1:?用法: upgrade-central.sh <fleetd 二进制> [catalog.json]}
CATALOG=${2:-}
HERE=$(cd "$(dirname "$0")" && pwd)
TS=$(date +%Y%m%d%H%M%S)
PREFIX=/opt/fleetd
ETC=/etc/fleetd
URL=http://127.0.0.1:25900/v2/api/v1/system
say() { echo "== $*"; }
die() { echo "错误：$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "需要 root"
[ -L "$PREFIX/current" ] || die "还没装过 fleetd（先用 install-central.sh）"
[ -f "$ETC/fleetd.toml" ] || die "缺 $ETC/fleetd.toml"
for f in fleetd.service fetch-dbs.sh fleetd-ipintel-fetch.service fleetd-ipintel-fetch.timer; do
  [ -f "$HERE/$f" ] || die "$f 不在 $HERE"
done
chmod 755 "$BIN"
NEWVER=$("$BIN" version | awk '{print $2}') || die "$BIN 无法运行"
PREV=$(readlink "$PREFIX/current")
OLDVER=$("$PREFIX/current/fleetd" version 2>/dev/null | awk '{print $2}' || echo "?")
say "升级 fleetd $OLDVER → $NEWVER（当前 $PREV）"

say "用现有配置自检新二进制"
install -d -m 755 "$PREFIX/releases/$TS" "$PREFIX/releases/$TS/ipintel"
install -m 755 "$BIN" "$PREFIX/releases/$TS/fleetd"
if [ -n "$CATALOG" ]; then
  install -m 644 "$CATALOG" "$PREFIX/releases/$TS/catalog.json"
elif [ -f "$PREFIX/current/catalog.json" ]; then
  install -m 644 "$PREFIX/current/catalog.json" "$PREFIX/releases/$TS/catalog.json"
fi
install -m 644 "$HERE/fetch-dbs.sh" "$PREFIX/releases/$TS/ipintel/fetch-dbs.sh"
if ! runuser -u fleetd -- "$PREFIX/releases/$TS/fleetd" check-config --config "$ETC/fleetd.toml"; then
  rm -rf "$PREFIX/releases/$TS"
  die "新二进制不接受现有配置，线上未改动"
fi

say "systemd unit"
install -m 644 "$HERE/fleetd.service" /etc/systemd/system/fleetd.service
install -m 644 "$HERE/fleetd-ipintel-fetch.service" /etc/systemd/system/fleetd-ipintel-fetch.service
install -m 644 "$HERE/fleetd-ipintel-fetch.timer" /etc/systemd/system/fleetd-ipintel-fetch.timer
systemctl daemon-reload

say "切换 current → releases/$TS"
ln -sfn "releases/$TS" "$PREFIX/current.new" && mv -Tf "$PREFIX/current.new" "$PREFIX/current"

say "IP 情报离线库"
if [ ! -f /var/lib/fleetd/ipintel/MANIFEST ]; then
  AVAIL=$(df -Pm /var/lib | awk 'NR==2 {print $4}')
  if [ "${AVAIL:-0}" -lt 600 ]; then
    echo "警告：/var/lib 只剩 ${AVAIL} MB，跳过下载（需要约 400 MB 余量）；ipintel 照常启动但情报字段为空"
  elif systemctl start fleetd-ipintel-fetch.service; then
    echo "已下载：$(du -sh /var/lib/fleetd/ipintel | cut -f1)"
  else
    echo "警告：下载失败（不阻塞升级）"; journalctl -u fleetd-ipintel-fetch -n 20 --no-pager || true
  fi
else
  echo "已存在（$(du -sh /var/lib/fleetd/ipintel | cut -f1)），跳过"
fi
systemctl enable --now fleetd-ipintel-fetch.timer >/dev/null 2>&1 || true

verify() { # 期望版本
  for i in $(seq 1 30); do
    if OUT=$(curl -fsS -m 3 "$URL" 2>/dev/null) && echo "$OUT" | grep -q "\"version\":\"$1\""; then
      echo "$OUT" | python3 -c '
import json,sys; d=json.load(sys.stdin)["data"]
ov={p["resource"]:p for p in d["polls"]}.get("overview",{})
ii=d.get("ipintel") or {}
print("fleetd %s pid %s rss %.1f MB (anon %s MB)  overview v%s errors %s  ipintel %s loaded=%s backfill=%s" % (d["version"], d["pid"], d["rss_mb"], d.get("rss_anon_mb"),
      ov.get("version"), ov.get("errors"), ii.get("enabled"), ii.get("loaded"), (ii.get("state") or {}).get("backfill")))'
      return 0
    fi
    sleep 1
  done
  return 1
}

say "重启并核对"
systemctl restart fleetd
if verify "$NEWVER"; then
  (cd "$PREFIX/releases" && ls -1 | sort | head -n -5 | xargs -r rm -rf)
  echo "OK：fleetd $NEWVER 已运行（上一版保留在 $PREFIX/$PREV，手动回退：bash rollback.sh --previous）"
  exit 0
fi
echo "新版本 30 s 内没有正常响应，自动回退到 $PREV" >&2
journalctl -u fleetd --no-pager -n 30 >&2 || true
# 旧版本不认当前配置（这次一起改了配置、用了新配置值）→ 换回最近一份它认的 fleetd.toml.before-* 备份
if ! runuser -u fleetd -- "$PREFIX/$PREV/fleetd" check-config --config "$ETC/fleetd.toml" >/dev/null 2>&1; then
  for b in $(ls -1t "$ETC"/fleetd.toml.before-* 2>/dev/null); do
    if runuser -u fleetd -- "$PREFIX/$PREV/fleetd" check-config --config "$b" >/dev/null 2>&1; then
      cp -p "$ETC/fleetd.toml" "$ETC/fleetd.toml.rolledback-$TS" && cp -p "$b" "$ETC/fleetd.toml"
      echo "旧版本不接受当前配置：已换回 $b（新配置留在 fleetd.toml.rolledback-$TS）" >&2
      break
    fi
  done
fi
ln -sfn "$PREV" "$PREFIX/current.new" && mv -Tf "$PREFIX/current.new" "$PREFIX/current"
systemctl restart fleetd
if verify "$OLDVER"; then
  die "已回退到 $OLDVER（$PREV），新版本目录保留在 $PREFIX/releases/$TS 供排查"
fi
die "回退后仍无响应，请查看 journalctl -u fleetd"
