#!/bin/bash
# 家里 PVE 上每天从生产中控取一份最新备份（fleet-backup-pull.timer 触发，root 执行），存到 /var/backups/fleet-prod，保留 30 份，0600。
# 中控那头是 fleet-backup 用户的 forced command：只会把最新一份备份 cat 出来，别的什么也做不了。
# 中控开了 age 加密（/etc/fleet/backup-age.recipients）时取到的是密文，存成 .tar.age；PVE 上没有私钥，只查文件头。
# 用法：FLEET_BACKUP_HOST=fleet-backup@<中控地址> bash fleet-backup-pull.sh     私钥默认 /root/.ssh/fleet-backup（FLEET_BACKUP_KEY 可改）
# 端口等其它 SSH 设置写进 /root/.ssh/config。第一次连接自动记下中控的主机指纹，之后指纹变了会拒绝。安装与恢复见 docs/OPS.md「备份与恢复」。
set -euo pipefail
HOST=${FLEET_BACKUP_HOST:?设置 FLEET_BACKUP_HOST=fleet-backup@<中控地址>}
KEY=${FLEET_BACKUP_KEY:-/root/.ssh/fleet-backup}
D=/var/backups/fleet-prod
KEEP=30
umask 077
install -d -m 700 "$D"
TMP=$D/.pull.tar
trap 'rm -f "$TMP"' EXIT
ssh -n -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=30 "$HOST" latest > "$TMP"
if [ "$(head -c 21 "$TMP" | tr -d '\0')" = "age-encryption.org/v1" ]; then
  # 密文：解不开，只能确认是 age 文件且不是空壳（tar 至少 10 KB）
  EXT=tar.age
  [ "$(wc -c < "$TMP")" -gt 10240 ] || { echo "取到的加密备份太小：$(wc -c < "$TMP") 字节"; exit 1; }
else
  # tar 要能完整读完，且库和密钥都在
  EXT=tar
  members=$(tar -tf "$TMP")
  grep -qx fleet.dump <<<"$members" && grep -qx etc/fleet/secret.key <<<"$members" || { echo "取到的备份不完整：$(wc -c < "$TMP") 字节"; exit 1; }
fi
last=$(ls -1 "$D"/fleet-prod-*.tar* 2>/dev/null | tail -n 1 || true)   # 第一次还没有
if [ -n "$last" ] && cmp -s "$TMP" "$last"; then
  echo "中控的备份没有更新（和上次取到的 $last 一样）：去中控看 systemctl status fleet-backup"
  exit 1
fi
NAME=fleet-prod-$(TZ=Asia/Shanghai date +%Y%m%d-%H%M%S).$EXT
mv "$TMP" "$D/$NAME"
ls -1 "$D"/fleet-prod-*.tar* | head -n -"$KEEP" | xargs -r rm -f
echo "已取到 $D/$NAME（$(du -h "$D/$NAME" | cut -f1)），共 $(ls -1 "$D"/fleet-prod-*.tar* | wc -l) 份"
