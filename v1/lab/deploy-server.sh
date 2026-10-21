#!/bin/bash
# 在 fleet-dev 上编译并（重新）部署 fleet-server。幂等：首次运行会建用户、目录、密钥、配置、管理员账号。
# 管理员初始密码写在 fleet-dev:/root/fleet-admin-password（0600），不打印。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
bash "$HERE/sync-server.sh"
# 远端目录没有 .git：提交哈希在本机取，经环境变量交给远端的 cargo，agent 与中控的版本号成为 0.1.0+<哈希>
GIT_HASH=$(git -C "$HERE" rev-parse --short HEAD)
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "FLEET_GIT_HASH=$GIT_HASH bash -s" <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
apt-get install -y -qq sshpass openssh-client >/dev/null && apt-get clean   # 不留 .deb 缓存
cd /root/src && cargo build -p fleet-server --release 2>&1 | tail -1
cargo build -p fleet-agent --release --target x86_64-unknown-linux-musl 2>&1 | tail -1
install -d -m 755 /opt/fleet/dist
install -m 755 target/x86_64-unknown-linux-musl/release/fleet-agent /opt/fleet/dist/fleet-agent
echo "fleet-agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)"
id fleet >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet
usermod -aG systemd-journal fleet   # fleetctl server-logs 需要读本机 journal
install -d -m 750 -o fleet -g fleet /etc/fleet /etc/fleet/ssh /var/lib/fleet
install -d -m 755 /opt/fleet /opt/fleet/dist /opt/fleet/web
[ -f /etc/fleet/secret.key ] || { /root/src/target/release/fleet-server gen-key /etc/fleet/secret.key; chown fleet:fleet /etc/fleet/secret.key; }
[ -f /etc/fleet/ssh/id_ed25519 ] || { ssh-keygen -q -t ed25519 -N "" -C fleet-central-lab -f /etc/fleet/ssh/id_ed25519; chown fleet:fleet /etc/fleet/ssh/id_ed25519*; }
touch /var/lib/fleet/known_hosts; chown fleet:fleet /var/lib/fleet/known_hosts
[ -f /etc/fleet/fleet.toml ] || cat > /etc/fleet/fleet.toml <<'TOML'
listen = "0.0.0.0:8080"
database_url = "postgres://fleet:fleet@127.0.0.1/fleet"
secret_key_file = "/etc/fleet/secret.key"
public_url = "http://192.168.31.60:8080"
web_dir = "/opt/fleet/web"
ssh_key = "/etc/fleet/ssh/id_ed25519"
known_hosts = "/var/lib/fleet/known_hosts"
dist_dir = "/opt/fleet/dist"
job_workers = 8

[retention]
raw_days = 3
access_days = 7

[numbering]
auto_compact = true
TOML
chown fleet:fleet /etc/fleet/fleet.toml; chmod 640 /etc/fleet/fleet.toml
install -m 755 /root/src/target/release/fleet-server /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server
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
systemctl daemon-reload
systemctl enable -q fleet-server
systemctl restart fleet-server
sleep 2
if [ ! -f /root/fleet-admin-password ]; then
  (umask 077; head -c 18 /dev/urandom | base64 | tr -d '/+=' > /root/fleet-admin-password)
  /opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml admin < /root/fleet-admin-password
fi
systemctl is-active fleet-server
curl -s -o /dev/null -w 'api=%{http_code}\n' http://127.0.0.1:8080/api/v1/auth/me
REMOTE
