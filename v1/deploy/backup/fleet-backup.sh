#!/bin/bash
# 中控每天的备份（fleet-backup.timer 触发，root 执行；也可手动 systemctl start fleet-backup）。装在 /opt/fleet/backup/。
# 一份备份是一个 tar：fleet.dump（pg_dump -Fc）+ etc/fleet（secret.key、fleet.toml、中控 SSH 密钥等）+ var/lib/fleet/known_hosts，
# 放在 /var/backups/fleet，保留最近 14 份。没有 secret.key，库里的 SS 密码、REALITY 私钥、通知渠道配置都解不开；
# 没有中控 SSH 密钥就登不上机器：所以和库打在同一个包里。
# 四张原始时序表（按天分区、只保留几天、量最大）只备结构不备数据；小时汇总等其余表全量。
# 家里 PVE 每天经 fleet-backup 用户取走最新一份（fleet-backup-latest.sh / fleet-backup-pull.sh）。恢复见 docs/OPS.md「备份与恢复」。
set -euo pipefail
D=/var/backups/fleet
KEEP=14
NAME=fleet-$(TZ=Asia/Shanghai date +%Y%m%d-%H%M%S).tar
# 临时目录放在同一个文件系统：/tmp 可能是 tmpfs，最后一步 mv 也是原子的
W=$(mktemp -d "$D/.work-XXXXXX")
trap 'rm -rf "$W"' EXIT
runuser -u postgres -- pg_dump -Fc -d fleet \
  --exclude-table-data-and-children=host_metric --exclude-table-data-and-children=entry_traffic \
  --exclude-table-data-and-children=probe_result --exclude-table-data-and-children=access_event > "$W/fleet.dump"
tar -cf "$W/$NAME" -C "$W" fleet.dump -C / etc/fleet var/lib/fleet/known_hosts
chgrp fleet-backup "$W/$NAME"
chmod 640 "$W/$NAME"   # fleet-backup 用户（PVE 拉取）要能读
mv "$W/$NAME" "$D/$NAME"
ls -1 "$D"/fleet-*.tar | head -n -"$KEEP" | xargs -r rm -f
echo "已备份 $D/$NAME（$(du -h "$D/$NAME" | cut -f1)），共 $(ls -1 "$D"/fleet-*.tar | wc -l) 份"
