#!/bin/bash
# prodcopy.sh — a395 上用「中控数据库副本」跑一个与生产同形态的 fleetd（native、komari 关、ipintel 开），用于内存 / 启动 / 告警验收。
#   fleet/staging/prodcopy.sh fetch                 从中控做一致性备份（python sqlite3 backup，nice/ionice）→ 经本机流式拷到 a395（0700 目录），中控不留副本
#   fleet/staging/prodcopy.sh start [BIN]           用副本的全新拷贝起实例（默认 BIN = fleet-build 的 release 构建）；127.0.0.1:18376，auth = dev-allow-local
#   fleet/staging/prodcopy.sh load [秒]             模拟一个打开界面的浏览器（SSE + 生产 /system.requests 里出现过的读路径），每 30 s 打印内存
#   fleet/staging/prodcopy.sh mem [QUERY]           GET /v2/api/v1/debug/mem（profile 构建可带 drop=…）
#   fleet/staging/prodcopy.sh alerts | logs | stop
# INST=b PORT=18377 可再起一个实例（前后版本并排比较）；XENV='MIMALLOC_PURGE_DELAY=0' 之类给实例加环境变量。a395 的 THP 是 always（中控是 madvise）：实例用 prctl(PR_SET_THP_DISABLE) 启动，
# 否则匿名内存被 2 MB 大页撑高、和中控不可比。
# 副本只在 a395 的 ~/team-ops-eval/fleet-memfix/（0700）；实例不对外连接（无通知渠道、影子模式、不连 Komari / 旧后端）。
set -euo pipefail
A395=${A395:-kismet@10.172.17.213}
KEY=${KEY:-$HOME/.ssh/a395_kismet}
CENTRAL=${CENTRAL:-central}
HERE=$(cd "$(dirname "$0")" && pwd)
R='~/team-ops-eval/fleet-memfix'
INST=${INST:-a}
PORT=${PORT:-18376}
SSH=(ssh -i "$KEY" -o ConnectTimeout=10 "$A395")
case "${1:-}" in
fetch)
  "${SSH[@]}" "mkdir -p $R/prodcopy && chmod 700 $R && rm -f $R/prodcopy/*.db"
  ssh "$CENTRAL" 'set -e; D=$(mktemp -d /root/fleetd-copy.XXXX); trap "rm -rf $D" EXIT; cd $D
    nice -n 19 ionice -c3 python3 -c "
import sqlite3,sys
for n in [\"fleetd.db\",\"ingest.db\",\"ipintel.db\",\"legacy.db\"]:
    s=sqlite3.connect(\"file:/var/lib/fleetd/%s?mode=ro\"%n, uri=True); d=sqlite3.connect(n); s.backup(d); d.close(); s.close()
" && nice -n 19 tar cf - *.db | nice -n 19 gzip -1' | "${SSH[@]}" "tar xzf - -C $R/prodcopy && ls -la $R/prodcopy"
  ;;
start)
  BIN=${2:-'~/team-ops-eval/fleet-build/src/target/x86_64-unknown-linux-musl/release/fleetd'}
  tar --no-xattrs -C "$HERE" -czf - prodcopy_load.py | "${SSH[@]}" "tar xzf - -C $R"
  "${SSH[@]}" "set -e; cd $R; if [ -f pid-$INST ]; then kill \$(cat pid-$INST) 2>/dev/null && sleep 2 || true; fi; rm -rf run-$INST; mkdir -p run-$INST/data run-$INST/bin
    cp --reflink=auto prodcopy/*.db run-$INST/data/; cp $BIN run-$INST/bin/fleetd
    cat > run-$INST/fleetd.toml <<EOT
listen = \"127.0.0.1:$PORT\"
data_dir = \"\$HOME/team-ops-eval/fleet-memfix/run-$INST/data\"
catalog = \"\$HOME/team-ops-eval/fleet-stage/catalog.json\"
legacy_source = \"native\"
[komari]
enabled = false
[auth]
mode = \"dev-allow-local\"
cookie_secure = false
[ipintel]
enabled = true
db_dir = \"\$HOME/team-ops-eval/ipintel-db\"
state_db = \"\$HOME/team-ops-eval/fleet-memfix/run-$INST/data/ipintel.db\"
[edge]
native_probe = true
native_paths = true
EOT
    run-$INST/bin/fleetd check-config --config run-$INST/fleetd.toml >/dev/null
    cd run-$INST; FLEETD_LOG=info ${XENV:-} setsid nohup python3 -c 'import ctypes,os,sys; ctypes.CDLL(None).prctl(41,1,0,0,0); os.execv(sys.argv[1], sys.argv[1:])' bin/fleetd serve --config fleetd.toml > fleetd.log 2>&1 < /dev/null & echo \$! > ../pid-$INST; cd ..
    for i in \$(seq 1 60); do curl -fs -m 2 http://127.0.0.1:$PORT/v2/api/v1/system -o /dev/null && break; sleep 1; done
    curl -s http://127.0.0.1:$PORT/v2/api/v1/system | python3 -c 'import json,sys; d=json.load(sys.stdin)[\"data\"]; print(\"up\", d[\"version\"], d[\"legacy_source\"], \"anon\", d[\"rss_anon_mb\"])'"
  ;;
load) "${SSH[@]}" "cd $R && python3 prodcopy_load.py http://127.0.0.1:$PORT ${2:-600}" ;;
mem) "${SSH[@]}" "curl -s 'http://127.0.0.1:$PORT/v2/api/v1/debug/mem?${2:-}' | python3 -m json.tool" ;;
alerts) "${SSH[@]}" "grep -E 'notification' $R/run-$INST/fleetd.log | tail -n 40" ;;
logs) "${SSH[@]}" "tail -n 60 $R/run-$INST/fleetd.log" ;;
stop) "${SSH[@]}" "cd $R; [ -f pid-$INST ] && kill \$(cat pid-$INST) 2>/dev/null; rm -f pid-$INST; echo stopped" ;;
*) echo "usage: $0 fetch|start [BIN]|load [s]|mem [q]|alerts|logs|stop"; exit 2 ;;
esac
