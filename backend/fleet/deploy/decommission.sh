#!/bin/bash
# decommission.sh [--force] | --rollback — 下线旧系统（team-ops Python 后端），fleetd 原生接管（API.md §12）。以 root 在中控运行。
#
# 前提：fleetd ≥ 0.6.5 在跑，/var/lib/fleetd/legacy.db 已由 `fleetd legacy snapshot` 导入并持续原生喂数，
#       下线闸门 GET /v2/api/v1/shadow/legacy 的 paths.gate.passing = true（--force 跳过这一检查）。
# 步骤（任一步失败就停下，已做的步骤按提示用 --rollback 撤销）：
#   1. 核对闸门；
#   2. 停旧后端 team-ops.service（不再写 operations.db；文件与 release 全部保留）；
#   3. 停 fleetd → `fleetd legacy catchup`：快照之后人在旧系统里做的写入（资产/续费/密钥/草稿/任务/审计/确认）并入原生旧库；
#   4. /etc/fleetd/fleetd.toml 设 legacy_source = "native"，启动 fleetd，核对 /system 与 /v2/api/legacy/overview；
#   5. caddy-decommission.sh：/api/team-ops/*、/ops/api/* → fleetd；/ 与 /admin* → /admin/v2/app/；
#   6. disable team-ops.service（保持停止）。
# 不动：team-ops-logrotate（给 Xray 访问日志限长，relayd 在读）、team-ops-path-probe 与 komari-relay-probe（过渡期探测来源）、Komari。
#
# --rollback：Caddy 还原 → legacy_source = "team-ops" → 重启 fleetd → enable --now team-ops。
#   注意：原生期间在新系统里做的写入不会回写到旧库。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
CONF=/etc/fleetd/fleetd.toml
FLEETD=/opt/fleetd/current/fleetd
URL=http://127.0.0.1:25900
say() { echo "== $*"; }
die() { echo "错误：$*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "需要 root"
[ -x "$FLEETD" ] || die "找不到 $FLEETD"
[ -f "$CONF" ] || die "缺 $CONF"

set_source() {
    local v=$1
    cp -p "$CONF" "$CONF.before-legacy-source-$(date +%Y%m%d%H%M%S)"
    if grep -q '^legacy_source *=' "$CONF"; then
        sed -i "s/^legacy_source *=.*/legacy_source = \"$v\"/" "$CONF"
    else
        # 顶层键必须在第一个 [段] 之前
        awk -v line="legacy_source = \"$v\"" 'BEGIN{done=0} /^\[/ && !done {print line; done=1} {print} END{if(!done) print line}' "$CONF" > "$CONF.new"
        install -m "$(stat -c %a "$CONF")" -o "$(stat -c %U "$CONF")" -g "$(stat -c %G "$CONF")" "$CONF.new" "$CONF"
        rm -f "$CONF.new"
    fi
    runuser -u fleetd -- "$FLEETD" check-config --config "$CONF" >/dev/null || die "check-config 不通过（$CONF）"
}

wait_up() {
    for _ in $(seq 1 30); do
        curl -fsS "$URL/v2/api/v1/system" >/dev/null 2>&1 && return 0
        sleep 1
    done
    return 1
}

if [ "${1:-}" = "--rollback" ]; then
    say "Caddy 还原"
    bash "$HERE/caddy-decommission.sh" --remove || true
    say "fleetd 切回 legacy_source = team-ops"
    set_source team-ops
    systemctl restart fleetd
    wait_up || die "fleetd 没起来"
    say "启动旧后端"
    systemctl enable --now team-ops.service
    echo "已回退。原生期间的写入（如有）不在旧库里，请按审计记录人工核对。"
    exit 0
fi

say "1. 下线闸门"
G=$(curl -fsS "$URL/v2/api/v1/shadow/legacy" | python3 -c 'import json,sys; g=json.load(sys.stdin)["data"]["paths"]["gate"]; print(g["passing"], g["rounds_in_window"], g["unexplained_in_window"], g.get("clean_for_s"))')
echo "passing rounds unexplained clean_for_s = $G"
if [ "${G%% *}" != "True" ] && [ "${1:-}" != "--force" ]; then
    die "闸门未通过（--force 可跳过，不建议）"
fi

say "2. 停旧后端 team-ops.service"
systemctl stop team-ops.service

say "3. 停 fleetd，并入快照之后的人工写入"
systemctl stop fleetd
"$FLEETD" legacy catchup || { systemctl start fleetd; die "catchup 失败：fleetd 已按原配置重启；旧后端仍停着，确认后 systemctl start team-ops 或重跑"; }

say "4. fleetd 切到 legacy_source = native"
set_source native
systemctl start fleetd
wait_up || die "fleetd 没起来（--rollback 撤销）"
SRC=$(curl -fsS "$URL/v2/api/v1/system" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"]["legacy_source"])')
[ "$SRC" = native ] || die "legacy_source 仍是 $SRC"

say "5. Caddy 路由"
bash "$HERE/caddy-decommission.sh"

say "6. 旧后端保持停止"
systemctl disable team-ops.service
echo "完成：旧后端已下线，fleetd 原生提供 /v2/api/legacy/*、/api/team-ops/*、/ops/api/*。回退：bash $0 --rollback"
