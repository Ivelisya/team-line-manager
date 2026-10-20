#!/bin/bash
# caddy-route.sh [--remove] — 在 /etc/caddy/Caddyfile 里加入（或移除）fleetd 的两条路由（各自幂等，缺哪条补哪条）：
#
#         # fleetd /v2/api (phase 1a)
#         handle /v2/api/* {
#             reverse_proxy 127.0.0.1:25900 {
#                 flush_interval -1
#             }
#         }
#         # end fleetd /v2/api
#         # fleetd /v2/ingest
#         handle /v2/ingest/* {                     # nodeagent / relayd 上报（阶段 2）
#             request_body {
#                 max_size 256KB
#             }
#             reverse_proxy 127.0.0.1:25900
#         }
#         # end fleetd /v2/ingest
#
# 插在 `# team-ops-ui v2 (read-only preview)` 块之前（有的话），否则插在 `@admin path …` 锚点行之前；
# 都在 route{} 里、兜底 `handle { reverse_proxy 127.0.0.1:25774 }` 之前。幂等：已存在就不改。
# 每次修改前备份 Caddyfile.before-fleetd-<ts>，caddy validate 通过才替换，然后 systemctl reload caddy。
# 以 root 在中控运行。
set -euo pipefail
CADDYFILE=${CADDYFILE:-/etc/caddy/Caddyfile}
SITE=${SITE:-https://38.58.57.22}
MODE=add
[ "${1:-}" = "--remove" ] && MODE=remove
TS=$(date +%Y%m%d%H%M%S)
[ "$(id -u)" = 0 ] || { echo "需要 root" >&2; exit 1; }
[ -f "$CADDYFILE" ] || { echo "找不到 $CADDYFILE" >&2; exit 1; }
NEW="$CADDYFILE.fleetd.new"
set +e
python3 - "$CADDYFILE" "$NEW" "$MODE" <<'PY'
import re, sys
src, dst, mode = sys.argv[1:4]
s = open(src).read()
BLOCKS = [
    ("        # fleetd /v2/api (phase 1a)\n", "        # end fleetd /v2/api\n", """        handle /v2/api/* {
            reverse_proxy 127.0.0.1:25900 {
                flush_interval -1
            }
        }
"""),
    ("        # fleetd /v2/ingest\n", "        # end fleetd /v2/ingest\n", """        handle /v2/ingest/* {
            request_body {
                max_size 256KB
            }
            reverse_proxy 127.0.0.1:25900
        }
"""),
]
ANCHORS = ("        # team-ops-ui v2 (read-only preview)\n", "        @admin path /admin /admin/* /terminal /terminal/\n")
changed = []
for start, end, body in BLOCKS:
    present = start in s
    if mode == "remove":
        if present:
            s = re.sub(r"[ \t]*" + re.escape(start.strip()) + r"\n.*?[ \t]*" + re.escape(end.strip()) + r"\n", "", s, count=1, flags=re.S)
            changed.append("移除 " + start.strip())
        continue
    if present:
        continue
    for anchor in ANCHORS:
        if s.count(anchor) == 1:
            s = s.replace(anchor, start + body + end + anchor)
            changed.append("加入 " + start.strip() + "（在 " + anchor.strip() + " 之前）")
            break
    else:
        print("找不到唯一的锚点（team-ops-ui v2 块或 @admin 行），不改", file=sys.stderr)
        sys.exit(2)
if not changed:
    print("路由已是目标状态，不改")
    sys.exit(3)
print("\n".join(changed))
open(dst, "w").write(s)
PY
rc=$?
set -e
[ $rc = 3 ] && exit 0
[ $rc = 0 ] || { rm -f "$NEW"; exit 1; }
if ! caddy validate --config "$NEW" --adapter caddyfile >/dev/null 2>"$NEW.err"; then
  cat "$NEW.err" >&2; rm -f "$NEW" "$NEW.err"; echo "caddy validate 失败，未修改" >&2; exit 1
fi
rm -f "$NEW.err"
cp -p "$CADDYFILE" "$CADDYFILE.before-fleetd-$TS"
cat "$NEW" > "$CADDYFILE"   # 原地写，保留属主与权限
rm -f "$NEW"
systemctl reload caddy
if [ "$MODE" = add ]; then WHAT=加入; else WHAT=移除; fi
echo "已${WHAT} fleetd 路由（备份 $CADDYFILE.before-fleetd-$TS）"

# 核对：经 Caddy 的 /v2/api 应由 fleetd 回答（未登录 → 401 JSON）；/admin 与 /admin/v2 照旧
code() { curl -sk -o /dev/null -m 8 -w '%{http_code}' "$SITE$1"; }
if [ "$MODE" = add ]; then
  BODY=$(curl -sk -m 8 "$SITE/v2/api/v1/system" || true)
  echo "GET /v2/api/v1/system（无会话）→ $(code /v2/api/v1/system) $BODY"
  echo "$BODY" | grep -q '请先登录管理员账户' || echo "警告：/v2/api 没有到达 fleetd（fleetd 在运行吗？）" >&2
else
  echo "GET /v2/api/v1/system → $(code /v2/api/v1/system)（应回到 Komari 兜底）"
fi
echo "GET /admin/ → $(code /admin/)   GET /admin/v2/ → $(code /admin/v2/)"
