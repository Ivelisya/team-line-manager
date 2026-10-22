#!/bin/bash
# 在生产中控机（Debian 13，root）上安装 / 升级 fleet-server。由 deploy.sh 连同安装包传上来执行，包就在本脚本所在目录。
# 第一次：装系统包，建 PostgreSQL 库与角色、fleet 用户、密钥（secret.key、中控 SSH 密钥）、fleet.toml、Caddy。
# 之后每次：只替换 fleet-server、dist、前端、ipintel 与备份脚本并重启 fleet-server；已有的配置和密钥不动。
# 换下来的旧 fleet-server 留在 /opt/fleet/fleet-server.prev（回滚见 docs/OPS.md §9）；重启后未登录访问 /api/v1/auth/me 不是 401
# 就打出最近日志、退出码 1。
# /etc/fleet/secret.key 丢了，库里所有 SS 密码、REALITY 私钥、通知渠道配置都解不开：每天的备份连它一起打包（/var/backups/fleet），
# 家里 PVE 每天取走一份，见 docs/OPS.md §10。
set -euo pipefail
DOMAIN=${1:?用法：install.sh <域名> [人用入口域名]}
PKG=$(cd "$(dirname "$0")" && pwd)
chmod 755 "$PKG/bin/fleet-server"   # 包经 Windows 中转会丢可执行位；首次安装要直接运行它生成密钥
export DEBIAN_FRONTEND=noninteractive

# Caddy 用官方源：Debian 自带的 2.6 没有 trusted_proxies / client_ip_headers，人用入口在 Cloudflare 后面要靠它们取真实 IP
if [ ! -f /etc/apt/sources.list.d/caddy-stable.list ]; then
  apt-get update -qq
  apt-get install -y -qq curl gpg debian-keyring debian-archive-keyring apt-transport-https >/dev/null
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --batch --yes --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq
  apt-get install -y -qq -o Dpkg::Options::=--force-confold caddy >/dev/null
fi

missing=$(for p in postgresql caddy sshpass openssh-client systemd-timesyncd curl jq; do dpkg -s "$p" >/dev/null 2>&1 || echo "$p"; done)
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
install -d -m 750 -o fleet -g fleet /etc/fleet /etc/fleet/ssh /var/lib/fleet
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
access_days = 7

[numbering]
auto_compact = true
TOML
  chown fleet:fleet /etc/fleet/fleet.toml
  chmod 640 /etc/fleet/fleet.toml
fi
[ -f /etc/fleet/secret.key ] || { "$PKG/bin/fleet-server" gen-key /etc/fleet/secret.key; chown fleet:fleet /etc/fleet/secret.key; chmod 600 /etc/fleet/secret.key; }
[ -f /etc/fleet/ssh/id_ed25519 ] || { ssh-keygen -q -t ed25519 -N "" -C "fleet-central@$DOMAIN" -f /etc/fleet/ssh/id_ed25519; chown fleet:fleet /etc/fleet/ssh/id_ed25519*; }
[ -f /var/lib/fleet/known_hosts ] || install -m 640 -o fleet -g fleet /dev/null /var/lib/fleet/known_hosts

# xray、ssserver、geoip.dat 只在与这台中控上的不同时才随包带来（deploy.sh 先比对 sha256），没带的保持原样
install -m 755 "$PKG/bin/fleet-agent" /opt/fleet/dist/
for f in xray ssserver; do [ ! -f "$PKG/bin/$f" ] || install -m 755 "$PKG/bin/$f" /opt/fleet/dist/; done
[ ! -f "$PKG/bin/geoip.dat" ] || install -m 644 "$PKG/bin/geoip.dat" /opt/fleet/dist/
# 换下来的旧版留作 fleet-server.prev（同一个二进制重复部署时不覆盖它），回滚见 docs/OPS.md §9
if [ -f /opt/fleet/fleet-server ] && ! cmp -s "$PKG/bin/fleet-server" /opt/fleet/fleet-server; then
  cp -p /opt/fleet/fleet-server /opt/fleet/fleet-server.prev
fi
install -m 755 "$PKG/bin/fleet-server" /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server
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
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/fleet
ProtectHome=yes
PrivateTmp=yes
[Install]
WantedBy=multi-user.target
UNIT

# IP 情报离线库：每月更新；首次下载约 190 MB，后台进行，fleet-server 5 分钟内自动加载
install -m 644 "$PKG/fetch-dbs.sh" /opt/fleet/ipintel/fetch-dbs.sh
install -d -m 750 -o fleet -g fleet /var/lib/fleet/ipintel
install -m 644 "$PKG/fleet-ipintel-fetch.service" "$PKG/fleet-ipintel-fetch.timer" /etc/systemd/system/

# 每天备份（docs/OPS.md §10）：北京时间 03:30 把库与密钥打包到 /var/backups/fleet，留 14 份。
# 家里 PVE 用 fleet-backup 用户取最新一份：/etc/fleet/backup.pub（PVE 的公钥）存在才写进它的 authorized_keys，
# 登录后只能执行 forced command（把最新一份 cat 出来）；文件由 root 持有，这个用户自己改不了
id fleet-backup >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet-backup --shell /bin/sh fleet-backup
install -d -m 750 -o root -g fleet-backup /var/backups/fleet
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

# Caddy 终止 TLS（Let's Encrypt 自动签发续期），整站反代到本机 fleet-server；长轮询、WebSocket、gzip 上报都不用额外配置。
# $DOMAIN 是 agent 回连与上架出口校验（/echo）用的，DNS 只能灰云直连。人用入口（第二个参数，给过一次就记在 /etc/fleet/panel_host）
# 在 Cloudflare 后面：只接受 CF 网段的连接，客户端 IP 取 CF-Connecting-IP，见 docs/OPS.md §11
PANEL=${2:-}
if [ -n "$PANEL" ]; then echo "$PANEL" > /etc/fleet/panel_host; else PANEL=$(cat /etc/fleet/panel_host 2>/dev/null || true); fi
CF_NETS="173.245.48.0/20 103.21.244.0/22 103.22.200.0/22 103.31.4.0/22 141.101.64.0/18 108.162.192.0/18 190.93.240.0/20 188.114.96.0/20 197.234.240.0/22 198.41.128.0/17 162.158.0.0/15 104.16.0.0/13 104.24.0.0/14 172.64.0.0/13 131.0.72.0/22 2400:cb00::/32 2606:4700::/32 2803:f800::/32 2405:b500::/32 2405:8100::/32 2a06:98c0::/29 2c0f:f248::/32"   # cloudflare.com/ips-v4、ips-v6
CADDY="$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}"
if [ -n "$PANEL" ]; then
  CADDY="{
	servers {
		trusted_proxies static $CF_NETS
		client_ip_headers CF-Connecting-IP
	}
}

$CADDY

$PANEL {
	@direct not remote_ip $CF_NETS
	respond @direct \"请经 Cloudflare 访问\" 403
	reverse_proxy 127.0.0.1:8080 {
		header_up X-Forwarded-For {client_ip}
	}
}"
fi
if [ "$(cat /etc/caddy/Caddyfile 2>/dev/null)" != "$CADDY" ]; then
  [ -f /etc/caddy/Caddyfile.dist ] || cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.dist
  printf '%s\n' "$CADDY" > /etc/caddy/Caddyfile.new
  if caddy validate --config /etc/caddy/Caddyfile.new --adapter caddyfile >/dev/null 2>&1; then
    mv /etc/caddy/Caddyfile.new /etc/caddy/Caddyfile
    systemctl reload caddy
  else
    echo "新 Caddyfile 校验不过，保留原来的（见 /etc/caddy/Caddyfile.new）"; exit 1
  fi
fi

# fleet-server 启动时先跑数据库迁移再监听：最多等 60 秒，未登录访问应回 401
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
ls /var/backups/fleet/fleet-*.tar >/dev/null 2>&1 || systemctl start --no-block fleet-backup.service   # 第一次装上备份：先做一份
echo "fleet-server $(systemctl is-active fleet-server)，api=$code，agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)"
