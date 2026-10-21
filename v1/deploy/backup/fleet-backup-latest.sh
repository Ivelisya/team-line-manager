#!/bin/sh
# fleet-backup 用户的 SSH forced command（install.sh 写进它的 authorized_keys：restrict,command="…"）。装在 /opt/fleet/backup/。
# 不管客户端要求执行什么，只把 /var/backups/fleet 里最新的一份备份写到标准输出。
set -eu
f=$(ls -1 /var/backups/fleet/fleet-*.tar 2>/dev/null | tail -n 1)
[ -n "$f" ] || { echo "中控上还没有备份" >&2; exit 1; }
exec cat "$f"
