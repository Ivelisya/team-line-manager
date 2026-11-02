#!/bin/bash
# 中控每天的备份（fleet-backup.timer 触发，root 执行；也可手动 systemctl start fleet-backup）。装在 /opt/fleet/backup/。
# 一份备份是一个 tar：fleet.dump（pg_dump -Fc）+ etc/fleet（secret.key、fleet.toml、中控 SSH 密钥等）+ var/lib/fleet/known_hosts，
# 再加中控 sshd 的 ed25519 主机密钥（软路由隧道按它的公钥做 StrictHostKeyChecking，新机器上换了就全连不上）与 Caddy 的带外配置
# etc/caddy/global.d、sites.d（如 PROXY 协议的 edge-proxy.caddy，丢了经边缘机的连接全断；这几样不存在就不打，不让备份失败），
# 放在 /var/backups/fleet，保留最近 14 天（每个北京日期只留最后一份，见文末的轮换）。没有 secret.key，库里的 SS 密码、REALITY 私钥、通知渠道配置都解不开；
# 没有中控 SSH 密钥就登不上机器：所以和库打在同一个包里。
# 四张原始时序表（按天分区、只保留几天、量最大）只备结构不备数据；小时汇总等其余表全量。
# 可选加密：/etc/fleet/backup-age.recipients（age 公钥，一行一个）存在且非空时，tar 经管道交给 age 加密后才落盘，存成 .tar.age；
# 私钥不放在中控和 PVE 上（docs/OPS.md §10）。没有这个文件照旧存明文 .tar。
# 每份旁边写一个 <备份名>.sha256（sha256sum 格式），PVE 拉取时核对，传输或磁盘上的损坏不会悄悄混进去。
# 家里 PVE 每天经 fleet-backup 用户取走最新一份（fleet-backup-latest.sh / fleet-backup-pull.sh）。恢复见 docs/OPS.md「备份与恢复」。
set -euo pipefail
D=/var/backups/fleet
KEEP=14
REC=/etc/fleet/backup-age.recipients
NAME=fleet-$(TZ=Asia/Shanghai date +%Y%m%d-%H%M%S).tar
# 临时目录放在同一个文件系统：/tmp 可能是 tmpfs，最后一步 mv 也是原子的
W=$(mktemp -d "$D/.work-XXXXXX")
trap 'rm -rf "$W"' EXIT
runuser -u postgres -- pg_dump -Fc -d fleet \
  --exclude-table-data-and-children=host_metric --exclude-table-data-and-children=entry_traffic \
  --exclude-table-data-and-children=probe_result --exclude-table-data-and-children=access_event > "$W/fleet.dump"
# 可选路径：存在才打进包（tar 遇到不存在的路径会失败）
OPT=()
for p in etc/ssh/ssh_host_ed25519_key etc/ssh/ssh_host_ed25519_key.pub etc/caddy/global.d etc/caddy/sites.d; do
  if [ -e "/$p" ]; then OPT+=("$p"); fi
done
if [ -s "$REC" ]; then
  NAME=$NAME.age
  tar -cf - -C "$W" fleet.dump -C / etc/fleet var/lib/fleet/known_hosts "${OPT[@]}" | age -R "$REC" > "$W/$NAME"
else
  tar -cf "$W/$NAME" -C "$W" fleet.dump -C / etc/fleet var/lib/fleet/known_hosts "${OPT[@]}"
fi
(cd "$W" && sha256sum "$NAME" > "$NAME.sha256")
chgrp fleet-backup "$W/$NAME" "$W/$NAME.sha256"
chmod 640 "$W/$NAME" "$W/$NAME.sha256"   # fleet-backup 用户（PVE 拉取）要能读
# 校验和先就位：拉取时看得到的备份一定有它的校验和
mv "$W/$NAME.sha256" "$D/$NAME.sha256"
mv "$W/$NAME" "$D/$NAME"
list() { ls -1 "$D"/fleet-*.tar* | grep -v '\.sha256$'; }
# 轮换：每个北京日期只留最后一份，只留最近 KEEP 个日期（同一天上线前的同步备份、手动备份不挤占 14 天的覆盖）；
# 另外总是保留最新 3 份（同一天部署两次、或部署后不久定时备份跑了，部署前那份迁移前的库还在）。
# 只认 fleet-<8 位日期>-<6 位时间>.tar[.age]；别的名字不动
declare -A last=()
while read -r f; do
  b=${f##*/}
  [[ $b =~ ^fleet-([0-9]{8})-[0-9]{6}\.tar(\.age)?$ ]] && last[${BASH_REMATCH[1]}]=$f
done < <(list)
mapfile -t days < <(printf '%s\n' "${!last[@]}" | sort | tail -n "$KEEP")
declare -A keep=()
for d in "${days[@]}"; do keep[${last[$d]}]=1; done
while read -r f; do keep[$f]=1; done < <(list | grep -E '/fleet-[0-9]{8}-[0-9]{6}\.tar(\.age)?$' | tail -n 3)
while read -r f; do
  [[ ${f##*/} =~ ^fleet-[0-9]{8}-[0-9]{6}\.tar(\.age)?$ ]] || continue
  [ -n "${keep[$f]:-}" ] || rm -f "$f" "$f.sha256"
done < <(list)
echo "已备份 $D/$NAME（$(du -h "$D/$NAME" | cut -f1)），共 $(list | wc -l) 份"
