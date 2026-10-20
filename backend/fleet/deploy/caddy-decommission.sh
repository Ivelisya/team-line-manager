#!/bin/bash
# caddy-decommission.sh [--remove] — 旧系统（team-ops Python 后端 + Komari 插件）下线时的 Caddy 路由（API.md §12）。
#
# 加入（幂等）：
#   1. 旧入口转给 fleetd 的兼容层（插在 `# fleetd /v2/api (phase 1a)` 块之前）：
#          # fleetd decommission
#          handle /api/team-ops/* { reverse_proxy 127.0.0.1:25900 }   # 旧界面会话接口
#          handle /ops/api/*      { reverse_proxy 127.0.0.1:25900 }   # X-Api-Key（tlm_…，旧密钥继续有效）
#          # end fleetd decommission
#   2. 首页与旧后台跳到新界面：`redir @home /admin/home 302` 改为 `redir @home /admin/v2/app/ 302`；
#      在 `@admin path …` 行之前加 `/admin`、`/admin/`、`/admin/*`（/admin/v2/* 在前面已被新界面接走）→ /admin/v2/app/。
# --remove 还原以上改动。每次修改前备份 Caddyfile.before-decommission-<ts>，caddy validate 通过才替换，然后 reload。
# DRY_RUN=1：只生成并校验新文件、打印 diff，不替换不重载。
# 以 root 在中控运行；先把 fleetd 切到 legacy_source = "native" 并确认 /v2/api/v1/system 的 legacy_source 为 native。
set -euo pipefail
CADDYFILE=${CADDYFILE:-/etc/caddy/Caddyfile}
MODE=add
[ "${1:-}" = "--remove" ] && MODE=remove
TS=$(date +%Y%m%d%H%M%S)
[ "$(id -u)" = 0 ] || { echo "需要 root" >&2; exit 1; }
[ -f "$CADDYFILE" ] || { echo "找不到 $CADDYFILE" >&2; exit 1; }
NEW="$CADDYFILE.decommission.new"
set +e
python3 - "$CADDYFILE" "$NEW" "$MODE" <<'PY'
import re, sys
src, dst, mode = sys.argv[1:4]
s = open(src).read()
changed = []
START, END = "        # fleetd decommission\n", "        # end fleetd decommission\n"
BODY = """        handle /api/team-ops/* {
            reverse_proxy 127.0.0.1:25900
        }
        handle /ops/api/* {
            reverse_proxy 127.0.0.1:25900
        }
"""
ANCHOR = "        # fleetd /v2/api (phase 1a)\n"
HOME_OLD, HOME_NEW = "        redir @home /admin/home 302\n", "        redir @home /admin/v2/app/ 302\n"
ADM_START, ADM_END = "        # fleetd decommission admin\n", "        # end fleetd decommission admin\n"
ADM_BODY = """        @oldadmin path /admin /admin/ /admin/*
        redir @oldadmin /admin/v2/app/ 302
"""
ADM_ANCHOR = "        @admin path /admin /admin/* /terminal /terminal/\n"
if mode == "add":
    if START not in s:
        if s.count(ANCHOR) != 1:
            print("找不到唯一的 `# fleetd /v2/api (phase 1a)` 锚点（先跑 caddy-route.sh）", file=sys.stderr); sys.exit(2)
        s = s.replace(ANCHOR, START + BODY + END + ANCHOR); changed.append("加入 /api/team-ops/* 与 /ops/api/* → fleetd")
    if HOME_OLD in s:
        s = s.replace(HOME_OLD, HOME_NEW); changed.append("首页 → /admin/v2/app/")
    if ADM_START not in s:
        if s.count(ADM_ANCHOR) != 1:
            print("找不到唯一的 @admin 锚点", file=sys.stderr); sys.exit(2)
        s = s.replace(ADM_ANCHOR, ADM_START + ADM_BODY + ADM_END + ADM_ANCHOR); changed.append("/admin* → /admin/v2/app/")
else:
    for a, b in ((START, END), (ADM_START, ADM_END)):
        if a in s:
            s = re.sub(r"[ \t]*" + re.escape(a.strip()) + r"\n.*?[ \t]*" + re.escape(b.strip()) + r"\n", "", s, count=1, flags=re.S)
            changed.append("移除 " + a.strip())
    if HOME_NEW in s:
        s = s.replace(HOME_NEW, HOME_OLD); changed.append("首页还原为 /admin/home")
if not changed:
    print("路由已是目标状态，不改"); sys.exit(3)
print("\n".join(changed))
open(dst, "w").write(s)
PY
rc=$?
set -e
[ $rc = 3 ] && exit 0
[ $rc = 0 ] || exit $rc
if ! caddy validate --config "$NEW" --adapter caddyfile >/dev/null 2>&1; then
    echo "caddy validate 不通过，不替换（新文件留在 $NEW）" >&2
    caddy validate --config "$NEW" --adapter caddyfile 2>&1 | tail -5 >&2
    exit 1
fi
if [ -n "${DRY_RUN:-}" ]; then
    echo "DRY_RUN：校验通过，新文件在 $NEW（未替换、未重载）"
    diff -u "$CADDYFILE" "$NEW" || true
    exit 0
fi
cp -p "$CADDYFILE" "$CADDYFILE.before-decommission-$TS"
install -m 0644 "$NEW" "$CADDYFILE"
rm -f "$NEW"
systemctl reload caddy
echo "已更新并重载 Caddy（备份 $CADDYFILE.before-decommission-$TS）"
