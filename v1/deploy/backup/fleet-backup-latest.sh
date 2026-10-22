#!/bin/sh
# fleet-backup 用户的 SSH forced command（install.sh 写进它的 authorized_keys：restrict,command="…"）。装在 /opt/fleet/backup/。
# 不管客户端要求执行什么，只把 /var/backups/fleet 里最新的一份备份写到标准输出（明文 .tar 或 age 加密的 .tar.age，原样输出）。
# 客户端请求 `latest+sha256`（fleet-backup-pull.sh）时先输出一行「sha256  备份名」（fleet-backup.sh 写的 .sha256；
# 这次改动之前的备份没有，当场算），再输出备份本身；请求别的（旧版拉取脚本发的 latest）照旧只输出备份。
set -eu
f=$(ls -1 /var/backups/fleet/fleet-*.tar* 2>/dev/null | grep -v '\.sha256$' | tail -n 1 || true)
[ -n "$f" ] || { echo "中控上还没有备份" >&2; exit 1; }
if [ "${SSH_ORIGINAL_COMMAND:-}" = "latest+sha256" ]; then
  if [ -s "$f.sha256" ]; then
    head -n 1 "$f.sha256"
  else
    (cd /var/backups/fleet && sha256sum "$(basename "$f")")
  fi
fi
exec cat "$f"
