#!/bin/bash
# 家里 PVE 上每天从生产中控取一份最新备份（fleet-backup-pull.timer 触发，root 执行），存到 /var/backups/fleet-prod，保留 30 份，0600。
# 中控那头是 fleet-backup 用户的 forced command：只会把最新一份备份 cat 出来，别的什么也做不了。
# 中控开了 age 加密（/etc/fleet/backup-age.recipients）时取到的是密文，存成 .tar.age；PVE 上没有私钥，只查文件头。
# 明文和密文都按中控备份时算的 sha256 核对（forced command 先输出一行「sha256  备份名」再输出备份），对不上不存；
# 校验和一并存成 <文件名>.sha256（按 PVE 上的文件名写），恢复前在那个目录里 sha256sum -c <文件名>.sha256 再核一次。
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
trap 'rm -f "$TMP" "$TMP.sum"' EXIT
# 第一行是校验和（bash 的 read 从管道逐字节读，读完这一行后面的字节原样留给 cat）；ssh 失败时 pipefail 让脚本退出
ssh -n -i "$KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=30 "$HOST" 'latest+sha256' |
  { IFS= read -r l || true; printf '%s\n' "$l" > "$TMP.sum"; cat > "$TMP"; }
sum=$(cat "$TMP.sum")
want=${sum%% *}
[[ $want =~ ^[0-9a-f]{64}$ ]] || { echo "中控没有先给出校验和（第一行不是 sha256）：中控的 fleet-backup-latest.sh 是旧版？在中控跑一次 deploy.sh"; exit 1; }
got=$(sha256sum "$TMP" | cut -d' ' -f1)
[ "$got" = "$want" ] || { echo "取到的备份和中控的校验和对不上（${sum#* }：中控 $want，取到 $got，$(wc -c < "$TMP") 字节），不存"; exit 1; }
if [ "$(head -c 21 "$TMP" | tr -d '\0')" = "age-encryption.org/v1" ]; then
  # 密文：解不开，只能确认是 age 文件且不是空壳（tar 至少 10 KB）
  EXT=tar.age
  [ "$(wc -c < "$TMP")" -gt 10240 ] || { echo "取到的加密备份太小：$(wc -c < "$TMP") 字节"; exit 1; }
else
  # tar 要能完整读完，且库和密钥都在
  EXT=tar
  members=$(tar -tf "$TMP")
  grep -qx fleet.dump <<<"$members" && grep -qx etc/fleet/secret.key <<<"$members" || { echo "取到的备份不完整：$(wc -c < "$TMP") 字节"; exit 1; }
  # sshd 主机密钥不在包里只警告不拒收（中控还没部署带它的备份脚本时旧备份就没有）：缺了它，新机器恢复后软路由隧道全连不上
  grep -qx etc/ssh/ssh_host_ed25519_key <<<"$members" || echo "注意：备份里没有 etc/ssh/ssh_host_ed25519_key（中控 sshd 主机密钥）：中控的备份脚本是旧版？在中控跑一次 deploy.sh" >&2
fi
list() { ls -1 "$D"/fleet-prod-*.tar* 2>/dev/null | grep -v '\.sha256$' || true; }
last=$(list | tail -n 1)   # 第一次还没有
if [ -n "$last" ] && cmp -s "$TMP" "$last"; then
  echo "中控的备份没有更新（和上次取到的 $last 一样）：去中控看 systemctl status fleet-backup"
  exit 1
fi
NAME=fleet-prod-$(TZ=Asia/Shanghai date +%Y%m%d-%H%M%S).$EXT
echo "$want  $NAME" > "$D/$NAME.sha256"
mv "$TMP" "$D/$NAME"
list | head -n -"$KEEP" | while read -r f; do rm -f "$f" "$f.sha256"; done
echo "已取到 $D/$NAME（$(du -h "$D/$NAME" | cut -f1)，sha256 已核对），共 $(list | wc -l) 份"
