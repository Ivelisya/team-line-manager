#!/bin/bash
# 在生产中控机（Debian 13，root）上安装 / 升级 fleet-server。由 deploy.sh 连同安装包传上来执行，包就在本脚本所在目录。
# 第一次：装系统包，建 PostgreSQL 库与角色、fleet 用户、密钥（secret.key、中控 SSH 密钥）、fleet.toml、Caddy。
# 之后每次：只替换 fleet-server、dist、前端、ipintel 脚本并重启 fleet-server；已有的配置和密钥不动。
# /etc/fleet/secret.key 丢了，库里所有 SS 密码、REALITY 私钥、通知渠道配置都解不开：首次部署后把它拷到中控机以外的地方保管。
set -euo pipefail
DOMAIN=${1:?用法：install.sh <域名>}
PKG=$(cd "$(dirname "$0")" && pwd)
chmod 755 "$PKG/bin/fleet-server" "$PKG/bin/fleet-agent" "$PKG/bin/xray" "$PKG/bin/ssserver"   # 包经 Windows 中转会丢可执行位
export DEBIAN_FRONTEND=noninteractive

missing=$(for p in postgresql caddy sshpass openssh-client systemd-timesyncd curl jq; do dpkg -s "$p" >/dev/null 2>&1 || echo "$p"; done)
if [ -n "$missing" ]; then
  apt-get update -qq
  apt-get install -y -qq $missing >/dev/null
  apt-get clean
fi
systemctl enable -q --now systemd-timesyncd postgresql caddy

id fleet >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet
usermod -aG systemd-journal fleet   # fleetctl server-logs 需要读本机 journal
install -d -m 750 -o fleet -g fleet /etc/fleet /etc/fleet/ssh /var/lib/fleet
install -d -m 755 /opt/fleet /opt/fleet/dist /opt/fleet/ipintel

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

install -m 755 "$PKG/bin/fleet-agent" "$PKG/bin/xray" "$PKG/bin/ssserver" /opt/fleet/dist/
install -m 644 "$PKG/bin/geoip.dat" /opt/fleet/dist/
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

systemctl daemon-reload
systemctl enable -q fleet-server fleet-ipintel-fetch.timer
systemctl start fleet-ipintel-fetch.timer
[ -n "$(ls -A /var/lib/fleet/ipintel)" ] || systemctl start --no-block fleet-ipintel-fetch.service
systemctl restart fleet-server

# Caddy 终止 TLS（Let's Encrypt 自动签发续期），整站反代到本机 fleet-server；长轮询、WebSocket、gzip 上报都不用额外配置
CADDY="$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}"
if [ "$(cat /etc/caddy/Caddyfile 2>/dev/null)" != "$CADDY" ]; then
  [ -f /etc/caddy/Caddyfile.dist ] || cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.dist
  printf '%s\n' "$CADDY" > /etc/caddy/Caddyfile
  caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1
  systemctl reload caddy
fi

for _ in $(seq 30); do
  code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/api/v1/auth/me || true)
  [ "$code" = 401 ] && break
  sleep 1
done
echo "fleet-server $(systemctl is-active fleet-server)，api=$code，agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)"
