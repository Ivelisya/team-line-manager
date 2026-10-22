#!/bin/bash
# 在生产中控机（Debian 13，root）上安装 / 升级 fleet-server。由 deploy.sh 连同安装包传上来执行，包就在本脚本所在目录。
# 第一次：装系统包，建 PostgreSQL 库与角色、fleet 用户、密钥（secret.key、中控 SSH 密钥）、fleet.toml、Caddy。
# 之后每次：只替换 fleet-server、dist、前端、ipintel 与备份脚本并重启 fleet-server；已有的配置和密钥不动。
# 每次还装 /usr/local/bin/fleetctl，健康检查过后建或修复它的全权密钥 central-local（仍有效不换），写给 agent 的 /root/AGENTS.md、
# /root/CLAUDE.md 与 fleet-ops 技能（docs/OPS.md §13：用 agent 从中控全量运维）。
# 换下来的旧 fleet-server 留在 /opt/fleet/fleet-server.prev（回滚见 docs/OPS.md §9）；重启后未登录访问 /api/v1/auth/me 不是 401
# 就打出最近日志、退出码 1。
# /etc/fleet/secret.key 丢了，库里所有 SS 密码、REALITY 私钥、通知渠道配置都解不开：每天的备份连它一起打包（/var/backups/fleet），
# 家里 PVE 每天取走一份，见 docs/OPS.md §10。
set -euo pipefail
DOMAIN=${1:?用法：install.sh <域名>}
PKG=$(cd "$(dirname "$0")" && pwd)
chmod 755 "$PKG/bin/fleet-server"   # 包经 Windows 中转会丢可执行位；首次安装要直接运行它生成密钥
export DEBIAN_FRONTEND=noninteractive

# Caddy 用官方源（Debian 13 自带的是 2022 年的 2.6.2）；Caddyfile 与证书原样保留
if [ ! -f /etc/apt/sources.list.d/caddy-stable.list ]; then
  apt-get update -qq
  apt-get install -y -qq curl gpg debian-keyring debian-archive-keyring apt-transport-https >/dev/null
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq
  apt-get install -y -qq -o Dpkg::Options::=--force-confold caddy >/dev/null
fi

missing=$(for p in postgresql caddy sshpass openssh-client systemd-timesyncd curl jq age; do dpkg -s "$p" >/dev/null 2>&1 || echo "$p"; done)
if [ -n "$missing" ]; then
  apt-get update -qq
  apt-get install -y -qq $missing >/dev/null
  apt-get clean
fi
systemctl enable -q --now systemd-timesyncd postgresql caddy

# docs/OPS.md §8：WAL 大半是 checkpoint 之后第一次改页写下的整页镜像。压缩整页镜像、checkpoint 间隔拉长到 15 分钟；reload 即生效
PGCONF="# 由 fleet 的 deploy/central/install.sh 写入，见 docs/OPS.md §8
wal_compression = lz4
checkpoint_timeout = 15min
max_wal_size = 2GB"
for d in /etc/postgresql/*/main/conf.d; do
  [ "$(cat "$d/fleet.conf" 2>/dev/null)" = "$PGCONF" ] || { printf '%s\n' "$PGCONF" > "$d/fleet.conf"; systemctl reload postgresql; }
done

id fleet >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet
usermod -aG systemd-journal fleet   # fleetctl server-logs 需要读本机 journal
# /etc/fleet 归 root、fleet 只读：fleet 的进程在这里摆不了符号链接，下面 root 的 chown、读 backup.pub 不会被引到别处。
# /var/lib/fleet 是 fleet 可写的，root 不在里面建文件、改属主：要建的东西用 runuser -u fleet 去建
install -d -m 750 -o root -g fleet /etc/fleet /etc/fleet/ssh
install -d -m 750 -o fleet -g fleet /var/lib/fleet
install -d -m 755 /opt/fleet /opt/fleet/dist /opt/fleet/ipintel /opt/fleet/backup

if [ ! -f /etc/fleet/fleet.toml ]; then
  # 库密码只写进 fleet.toml；SQL 走标准输入，不出现在进程列表里
  DBPW=$(head -c 24 /dev/urandom | base64 | tr -d '/+=\n')
  if runuser -u postgres -- psql -tAc "select 1 from pg_roles where rolname = 'fleet'" | grep -q 1; then
    printf "alter role fleet login password '%s';\n" "$DBPW" | runuser -u postgres -- psql -q -v ON_ERROR_STOP=1
  else
    printf "create role fleet login password '%s';\n" "$DBPW" | runuser -u postgres -- psql -q -v ON_ERROR_STOP=1
  fi
  runuser -u postgres -- psql -tAc "select 1 from pg_database where datname = 'fleet'" | grep -q 1 \
    || runuser -u postgres -- psql -q -c "create database fleet owner fleet"
  # docs/OPS.md §8：shared_buffers 取内存的 1/4（最多 4 GB），work_mem 16MB
  SB_MB=$(( $(awk '/MemTotal/ {print $2}' /proc/meminfo) / 4096 ))
  [ "$SB_MB" -gt 4096 ] && SB_MB=4096
  runuser -u postgres -- psql -q -c "alter system set shared_buffers = '${SB_MB}MB'" -c "alter system set work_mem = '16MB'"
  systemctl restart postgresql

  cat > /etc/fleet/fleet.toml <<TOML
listen = "127.0.0.1:8080"
database_url = "postgres://fleet:$DBPW@127.0.0.1/fleet"
secret_key_file = "/etc/fleet/secret.key"
public_url = "https://$DOMAIN"
web_dir = "/opt/fleet/web"
ssh_key = "/etc/fleet/ssh/id_ed25519"
known_hosts = "/var/lib/fleet/known_hosts"
dist_dir = "/opt/fleet/dist"
# 前面是本机 Caddy：客户端 IP 取它追加的 X-Forwarded-For 最后一项（/echo 出口校验、登录限速都靠它）
trust_forwarded_for = true
job_workers = 8

[retention]
raw_days = 3
# 访问记录的保留期在网页设置里改（OPS.md §13）；这里只是没改过时原始记录留几天
access_days = 7

[numbering]
auto_compact = true
TOML
  chown fleet:fleet /etc/fleet/fleet.toml
  chmod 640 /etc/fleet/fleet.toml
fi
[ -f /etc/fleet/secret.key ] || { "$PKG/bin/fleet-server" gen-key /etc/fleet/secret.key; chown fleet:fleet /etc/fleet/secret.key; chmod 600 /etc/fleet/secret.key; }
[ -f /etc/fleet/ssh/id_ed25519 ] || { ssh-keygen -q -t ed25519 -N "" -C "fleet-central@$DOMAIN" -f /etc/fleet/ssh/id_ed25519; chown fleet:fleet /etc/fleet/ssh/id_ed25519*; }
[ -f /var/lib/fleet/known_hosts ] || runuser -u fleet -- install -m 640 /dev/null /var/lib/fleet/known_hosts

# xray、ssserver、geoip.dat 只在与这台中控上的不同时才随包带来（deploy.sh 先比对 sha256），没带的保持原样
install -m 755 "$PKG/bin/fleet-agent" /opt/fleet/dist/
for f in xray ssserver; do [ ! -f "$PKG/bin/$f" ] || install -m 755 "$PKG/bin/$f" /opt/fleet/dist/; done
[ ! -f "$PKG/bin/geoip.dat" ] || install -m 644 "$PKG/bin/geoip.dat" /opt/fleet/dist/
# 换下来的旧版留作 fleet-server.prev（同一个二进制重复部署时不覆盖它），回滚见 docs/OPS.md §9
if [ -f /opt/fleet/fleet-server ] && ! cmp -s "$PKG/bin/fleet-server" /opt/fleet/fleet-server; then
  cp -p /opt/fleet/fleet-server /opt/fleet/fleet-server.prev
fi
install -m 755 "$PKG/bin/fleet-server" /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server
# 中控本机的 fleetctl（docs/OPS.md §13）：ssh 进中控的 agent 直接用，配置与全权密钥由下面健康检查之后的 local-admin 写
install -m 755 "$PKG/bin/fleetctl" /usr/local/bin/fleetctl.new && mv -f /usr/local/bin/fleetctl.new /usr/local/bin/fleetctl
rm -rf /opt/fleet/web.new /opt/fleet/web.old
cp -r "$PKG/web" /opt/fleet/web.new
[ -d /opt/fleet/web ] && mv /opt/fleet/web /opt/fleet/web.old
mv /opt/fleet/web.new /opt/fleet/web && rm -rf /opt/fleet/web.old

cat > /etc/systemd/system/fleet-server.service <<'UNIT'
[Unit]
Description=fleet-server（团队线路管理 v1）
After=network-online.target postgresql.service
Wants=network-online.target
[Service]
User=fleet
Group=fleet
ExecStart=/opt/fleet/fleet-server serve -c /etc/fleet/fleet.toml
Restart=always
RestartSec=3
Environment=RUST_LOG=info,sqlx=warn,tower_http=warn
# 沙箱（子进程 ssh、sshpass、ssh-keyscan、ssh-keygen、journalctl 同样受限）：只写 /var/lib/fleet，不要任何特权。
# PrivateDevices 仍留着 /dev/ptmx 与 /dev/pts（sshpass 要 pty）；journal 靠 systemd-journal 组读 /var/log/journal，不受 ProtectProc、ProtectKernelLogs 影响。
# 与 lab/deploy-server.sh 里的单元保持一致
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/fleet
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
CapabilityBoundingSet=
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
UMask=0077
[Install]
WantedBy=multi-user.target
UNIT

# IP 情报离线库：每月更新；首次下载约 190 MB，后台进行，fleet-server 5 分钟内自动加载
install -m 644 "$PKG/fetch-dbs.sh" /opt/fleet/ipintel/fetch-dbs.sh
runuser -u fleet -- install -d -m 750 /var/lib/fleet/ipintel
install -m 644 "$PKG/fleet-ipintel-fetch.service" "$PKG/fleet-ipintel-fetch.timer" /etc/systemd/system/

# 每天备份（docs/OPS.md §10）：北京时间 03:30 把库与密钥打包到 /var/backups/fleet，留 14 份。
# 家里 PVE 用 fleet-backup 用户取最新一份：/etc/fleet/backup.pub（PVE 的公钥）存在才写进它的 authorized_keys，
# 登录后只能执行 forced command（把最新一份 cat 出来）；文件由 root 持有，这个用户自己改不了。
# /etc/fleet/backup-age.recipients（age 公钥，一行一个）存在时备份用 age 加密成 .tar.age（上面装了 age 包）
id fleet-backup >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet-backup --shell /bin/sh fleet-backup
# 目录 0755：fleet-server 要看得到最新一份的时间（超过 26 小时开告警 central_backup_stale）；每份备份本身仍是 0640 root:fleet-backup
install -d -m 755 -o root -g fleet-backup /var/backups/fleet
install -m 644 "$PKG/fleet-backup.sh" /opt/fleet/backup/fleet-backup.sh
install -m 755 "$PKG/fleet-backup-latest.sh" /opt/fleet/backup/fleet-backup-latest.sh
install -m 644 "$PKG/fleet-backup.service" "$PKG/fleet-backup.timer" /etc/systemd/system/
if [ -f /etc/fleet/backup.pub ]; then
  install -d -m 755 /var/lib/fleet-backup /var/lib/fleet-backup/.ssh
  printf 'restrict,command="/opt/fleet/backup/fleet-backup-latest.sh" %s\n' "$(head -n 1 /etc/fleet/backup.pub)" > /var/lib/fleet-backup/.ssh/authorized_keys
  chmod 644 /var/lib/fleet-backup/.ssh/authorized_keys
fi

systemctl daemon-reload
systemctl enable -q fleet-server fleet-ipintel-fetch.timer fleet-backup.timer
systemctl start fleet-ipintel-fetch.timer fleet-backup.timer
[ -n "$(ls -A /var/lib/fleet/ipintel)" ] || systemctl start --no-block fleet-ipintel-fetch.service
systemctl restart fleet-server

# Caddy 终止 TLS（Let's Encrypt 自动签发续期），整站反代到本机 fleet-server；长轮询、WebSocket、gzip 上报都不用额外配置
# 管理接口默认是 localhost:2019、不鉴权，本机任何用户（包括 fleet）都能改写 Caddy 配置：改成 /run/caddy 下的 unix socket，
# 目录 0700 只有 caddy（与 root）进得去。官方源的 caddy.service 没有 RuntimeDirectory，由 drop-in 补上；
# systemctl reload caddy 以 caddy 用户执行 caddy reload，从 Caddyfile 读到这个地址
CADDY_ADMIN=unix//run/caddy/admin.sock
CADDY="{
	admin $CADDY_ADMIN
}

$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}"
CADDY_DROPIN="[Service]
RuntimeDirectory=caddy
RuntimeDirectoryMode=0700"
caddy_restart=0
if [ "$(cat /etc/systemd/system/caddy.service.d/fleet.conf 2>/dev/null)" != "$CADDY_DROPIN" ]; then
  install -d -m 755 /etc/systemd/system/caddy.service.d
  printf '%s\n' "$CADDY_DROPIN" > /etc/systemd/system/caddy.service.d/fleet.conf
  systemctl daemon-reload
  caddy_restart=1   # /run/caddy 要在启动时建
fi
if [ "$(cat /etc/caddy/Caddyfile 2>/dev/null)" != "$CADDY" ]; then
  [ -f /etc/caddy/Caddyfile.dist ] || cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.dist
  printf '%s\n' "$CADDY" > /etc/caddy/Caddyfile.new
  if caddy validate --config /etc/caddy/Caddyfile.new --adapter caddyfile >/dev/null 2>&1; then
    # caddy reload 按新 Caddyfile 里的管理地址去连：正在跑的 Caddy 还在旧地址上（第一次从 2019 换过来）就连不上，只能重启
    grep -qF "admin $CADDY_ADMIN" /etc/caddy/Caddyfile 2>/dev/null || caddy_restart=1
    mv /etc/caddy/Caddyfile.new /etc/caddy/Caddyfile
    [ "$caddy_restart" = 1 ] || systemctl reload caddy
  else
    echo "新 Caddyfile 校验不过，保留原来的（见 /etc/caddy/Caddyfile.new）"; exit 1
  fi
fi
[ "$caddy_restart" = 0 ] || systemctl restart caddy

# fleet-server 启动时先占端口、再跑数据库迁移（迁移期间连接先挂着，迁移完才应答）：最多等 60 秒，未登录访问应回 401
code=000
for _ in $(seq 60); do
  code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/api/v1/auth/me || true)
  [ "$code" = 401 ] && break
  sleep 1
done
if [ "$code" != 401 ]; then
  echo "fleet-server 没有正常启动（$(systemctl is-active fleet-server)）：未登录访问 /api/v1/auth/me 应回 401，实际 $code。最近日志："
  journalctl -u fleet-server -n 60 --no-pager || true
  echo "旧版本在 /opt/fleet/fleet-server.prev，回滚见 docs/OPS.md §9"
  exit 1
fi
ls /var/backups/fleet/fleet-*.tar* >/dev/null 2>&1 || systemctl start --no-block fleet-backup.service   # 第一次装上备份：先做一份

# 用 agent 从中控全量运维（docs/OPS.md §13）：建或修复 root 的 fleetctl 用的全权密钥 central-local（仍有效就不换，
# 正在干活的 agent 不会因部署失效），写 /root/.config/fleetctl/config.toml；给 agent 的说明 /root/AGENTS.md 与 /root/CLAUDE.md
# （同一份）、Claude Code 的 fleet-ops 技能。说明与技能每次部署覆盖，改仓库里的模板
/opt/fleet/fleet-server local-admin -c /etc/fleet/fleet.toml
sed -e "s|__SITE__|生产中控 $DOMAIN|" -e "s|__NOTE__|这是生产中控（$DOMAIN）：线上全部客户的线路都由它管。|" "$PKG/central-agents.md" > "$PKG/AGENTS.md"
for f in AGENTS.md CLAUDE.md; do install -m 644 "$PKG/AGENTS.md" "/root/$f"; done
install -D -m 644 "$PKG/fleet-ops-SKILL.md" /root/.claude/skills/fleet-ops/SKILL.md
fc=$(HOME=/root fleetctl --json whoami 2>/dev/null | jq -r '.username // empty' || true)
echo "fleet-server $(systemctl is-active fleet-server)，api=$code，agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)，本机 fleetctl ${fc:-不可用（见上面 local-admin 的输出）}"
